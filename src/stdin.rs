//! vm.stdin: streaming stdin into the guest. The blocking reader thread and
//! its channel belong to this run's World, not to the process.

use std::sync::mpsc;

use crate::guest_cb;
use crate::world::world_mut;

guest_cb!(stdin_read, |_args, rv, scope| {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    if world_mut(scope).stdin_eof {
        let empty = v8::String::new(scope, "").unwrap();
        resolver.resolve(scope, empty.into());
    } else {
        let waiter = v8::Global::new(scope, resolver);
        let world = world_mut(scope);
        world.stdin_waiters.push(waiter);
        start_stdin_thread(world);
    }
    rv.set(resolver.get_promise(scope).into());
});

/// Length of the prefix of `buf` that can go to the guest now. A multi-byte
/// UTF-8 sequence cut by a read boundary is held back for the next chunk;
/// invalid bytes are not held back and decode lossily.
fn complete_utf8_len(buf: &[u8]) -> usize {
    let n = buf.len();
    for back in 1..=n.min(3) {
        let b = buf[n - back];
        if b & 0xC0 == 0x80 {
            continue; // continuation byte: keep looking for its lead byte
        }
        let need = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 1,
        };
        return if need > back { n - back } else { n };
    }
    n
}

/// Lazily spawns the blocking stdin reader. It sends chunks down a channel
/// the event loop polls; on EOF it sends None and exits. No libc needed.
/// Chunks end on UTF-8 boundaries so the guest never sees a split character.
/// The channel lives in the World: when the run ends, the sender half is
/// dropped and the reader thread exits on its next write.
fn start_stdin_thread(world: &mut crate::world::World) {
    if world.stdin_thread.is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    let sender = tx.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 65536];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    if !pending.is_empty() {
                        let _ = sender.send(Some(std::mem::take(&mut pending)));
                    }
                    let _ = sender.send(None);
                    break;
                }
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let ready_len = complete_utf8_len(&pending);
                    if ready_len == 0 {
                        continue;
                    }
                    let rest = pending.split_off(ready_len);
                    let ready = std::mem::replace(&mut pending, rest);
                    if sender.send(Some(ready)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    world.stdin_thread = Some(tx);
    world.stdin_chunks = Some(rx);
}

/// Hands one chunk to exactly one waiting reader, or resolves everyone with
/// "" at EOF.
fn resolve_stdin_waiters(scope: &mut v8::HandleScope, chunk: Option<&[u8]>) {
    if let Some(bytes) = chunk {
        let resolver = world_mut(scope).stdin_waiters.pop();
        if let Some(resolver) = resolver {
            let r = resolver.open(scope);
            let s = v8::String::new_from_utf8(scope, bytes, v8::NewStringType::Normal).unwrap();
            r.resolve(scope, s.into());
        }
        return;
    }
    let waiters: Vec<v8::Global<v8::PromiseResolver>> =
        std::mem::take(&mut world_mut(scope).stdin_waiters);
    let empty = v8::String::new(scope, "").unwrap();
    for w in waiters {
        w.open(scope).resolve(scope, empty.into());
    }
}

/// Drains one chunk off the reader channel, if any waiter is due.
pub(crate) fn poll_stdin(scope: &mut v8::HandleScope) -> Option<i32> {
    let eof = {
        let world = world_mut(scope);
        if world.stdin_waiters.is_empty() {
            return None;
        }
        world.stdin_eof
    };
    if eof {
        resolve_stdin_waiters(scope, None);
        return None;
    }
    let chunk = world_mut(scope)
        .stdin_chunks
        .as_ref()
        .and_then(|rx| rx.try_recv().ok());
    match chunk {
        Some(Some(bytes)) => resolve_stdin_waiters(scope, Some(&bytes)),
        Some(None) => {
            world_mut(scope).stdin_eof = true;
            resolve_stdin_waiters(scope, None);
        }
        None => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_len_passes_complete_input() {
        assert_eq!(complete_utf8_len(b""), 0);
        assert_eq!(complete_utf8_len(b"abc"), 3);
        assert_eq!(complete_utf8_len("aé日😀".as_bytes()), "aé日😀".len());
    }

    #[test]
    fn utf8_len_holds_back_cut_sequences() {
        let two = "é".as_bytes(); // C3 A9
        assert_eq!(complete_utf8_len(&[b'a', two[0]]), 1);
        let three = "日".as_bytes();
        assert_eq!(complete_utf8_len(&three[..1]), 0);
        assert_eq!(complete_utf8_len(&three[..2]), 0);
        let four = "😀".as_bytes();
        for cut in 1..4 {
            assert_eq!(complete_utf8_len(&[b'x', b'y'].iter().chain(&four[..cut]).copied().collect::<Vec<_>>()), 2);
        }
    }

    #[test]
    fn utf8_len_does_not_hold_back_invalid_bytes() {
        assert_eq!(complete_utf8_len(&[0xFF]), 1);
        assert_eq!(complete_utf8_len(&[b'a', 0x80, 0x80]), 3);
        assert_eq!(complete_utf8_len(&[0xC0]), 1);
    }
}

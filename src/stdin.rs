//! vm.stdin: streaming stdin into the guest.
//!
//! Process stdin is genuinely process-global, so it is modeled that way: one
//! process-wide hub owns a single blocking reader thread (started on the
//! first subscription, never restarted) that keeps the UTF-8 boundary logic
//! of [complete_utf8_len] and parks every chunk in a hub buffer. A run
//! holds only its [StdinSub] subscription and pulls chunks out of the hub
//! while it polls: nothing is ever pushed at a run, so a chunk that arrives
//! while the reading run is on its way out simply stays buffered for the
//! next subscriber instead of dying with it. EOF is sticky. When several
//! runs read stdin concurrently, the first subscriber (front of the queue)
//! receives the chunks until it ends; the others' read() promises stay
//! pending.
//!
//! Caveats: once started, the reader thread keeps draining stdin into the
//! hub buffer even while no run is subscribed (that is what preserves data
//! across runs, at the cost of RAM instead of pipe backpressure), and it
//! holds the process stdin lock while blocked, so a later serve() call in
//! the same process waits for the reader to hit EOF first.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::guest_cb;
use crate::world::world_mut;

/// A run's subscription to the stdin hub. Dropping it releases the hub slot
/// so a later run can receive chunks.
pub(crate) struct StdinSub {
    id: u64,
}

impl Drop for StdinSub {
    fn drop(&mut self) {
        unsubscribe(self.id);
    }
}

struct HubState {
    /// Chunks read from process stdin, waiting to be pulled by the current
    /// subscriber (or kept for the next one).
    buf: VecDeque<Vec<u8>>,
    /// Sticky: once process stdin hit EOF, every future subscriber sees it.
    eof: bool,
    /// Subscriber queue; the front id is the run currently entitled to
    /// chunks.
    subs: VecDeque<u64>,
    started: bool,
}

static HUB: LazyLock<Mutex<HubState>> = LazyLock::new(|| {
    Mutex::new(HubState {
        buf: VecDeque::new(),
        eof: false,
        subs: VecDeque::new(),
        started: false,
    })
});

static NEXT_SUB_ID: AtomicU64 = AtomicU64::new(1);

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

/// The one blocking reader thread. It stays alive between runs, parking
/// chunks in the hub buffer, and exits after EOF.
fn reader_loop() {
    use std::io::Read;
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 65536];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => {
                let mut st = HUB.lock().unwrap();
                if !pending.is_empty() {
                    st.buf.push_back(std::mem::take(&mut pending));
                }
                st.eof = true;
                return;
            }
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                let ready_len = complete_utf8_len(&pending);
                if ready_len == 0 {
                    continue;
                }
                let rest = pending.split_off(ready_len);
                let ready = std::mem::replace(&mut pending, rest);
                HUB.lock().unwrap().buf.push_back(ready);
            }
        }
    }
}

fn unsubscribe(id: u64) {
    let mut st = HUB.lock().unwrap();
    if let Some(pos) = st.subs.iter().position(|sid| *sid == id) {
        st.subs.remove(pos);
    }
}

fn subscribe() -> StdinSub {
    let id = NEXT_SUB_ID.fetch_add(1, Ordering::Relaxed);
    let mut st = HUB.lock().unwrap();
    st.subs.push_back(id);
    if !st.started {
        st.started = true;
        std::thread::Builder::new()
            .name("v8vm-stdin".to_string())
            .spawn(reader_loop)
            .expect("spawn stdin reader");
    }
    StdinSub { id }
}

/// Pulls one event for `id`: `Some(Some(chunk))` is data, `Some(None)` is
/// EOF (the subscription is retired with it), `None` means nothing to take
/// right now - either the buffer is empty and stdin is not at EOF, or
/// another run is ahead in the subscriber queue.
fn take_event(id: u64) -> Option<Option<Vec<u8>>> {
    let mut st = HUB.lock().unwrap();
    if st.subs.front() != Some(&id) {
        return None;
    }
    if let Some(chunk) = st.buf.pop_front() {
        return Some(Some(chunk));
    }
    if st.eof {
        st.subs.pop_front();
        return Some(None);
    }
    None
}

guest_cb!(stdin_read, |_args, rv, scope| {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    {
        let world = world_mut(scope);
        if !world.stdin_eof && world.stdin_sub.is_none() {
            world.stdin_sub = Some(subscribe());
        }
    }
    if world_mut(scope).stdin_eof {
        let empty = v8::String::new(scope, "").unwrap();
        resolver.resolve(scope, empty.into());
    } else {
        let waiter = v8::Global::new(scope, resolver);
        world_mut(scope).stdin_waiters.push(waiter);
    }
    rv.set(resolver.get_promise(scope).into());
});

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

/// Delivers one hub event (if any) to the waiting readers.
pub(crate) fn poll_stdin(scope: &mut v8::HandleScope) -> Option<i32> {
    if world_mut(scope).stdin_waiters.is_empty() {
        return None;
    }
    if world_mut(scope).stdin_eof {
        resolve_stdin_waiters(scope, None);
        return None;
    }
    let sub_id = world_mut(scope).stdin_sub.as_ref().map(|sub| sub.id);
    let event = sub_id.and_then(take_event);
    match event {
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
            assert_eq!(complete_utf8_len(&b"xy".iter().chain(&four[..cut]).copied().collect::<Vec<_>>()), 2);
        }
    }

    #[test]
    fn utf8_len_does_not_hold_back_invalid_bytes() {
        assert_eq!(complete_utf8_len(&[0xFF]), 1);
        assert_eq!(complete_utf8_len(&[b'a', 0x80, 0x80]), 3);
        assert_eq!(complete_utf8_len(&[0xC0]), 1);
    }
}

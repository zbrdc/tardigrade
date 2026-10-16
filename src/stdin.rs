//! vm.stdin: streaming stdin into the guest.
//!
//! Process stdin is genuinely process-global, so it is modeled that way: one
//! process-wide hub owns a single blocking reader thread (started on the
//! first subscription, never restarted) that keeps the UTF-8 boundary logic
//! of [complete_utf8_len] and parks every chunk in a hub buffer. Consumers -
//! run event loops and serve sessions alike - pull chunks out of the hub:
//! nothing is ever pushed at a run, so a chunk that arrives while the
//! reading run is on its way out simply stays buffered for the next
//! subscriber instead of dying with it. EOF is sticky. The buffer has a
//! 1 MiB high-water mark: past it the reader thread blocks until chunks are
//! pulled, so a flooding writer cannot grow it without bound.
//!
//! Subscriptions are held only while needed: a run holds one while it has
//! pending vm.stdin readers (the event loop releases it once they drain),
//! and a serve session holds one for its lifetime while it waits for
//! commands. When several consumers read stdin concurrently, the front of
//! the subscriber queue receives the chunks until it unsubscribes; the
//! others' read() promises stay pending.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

use crate::guest_cb;
use crate::world::{world_mut, LoopWake};

/// Past this many buffered bytes the reader thread blocks until a consumer
/// pulls chunks (backpressure for e.g. `yes | tardigrade guest.js`).
const HIGH_WATER_MARK: usize = 1 << 20;

/// A run's or session's subscription to the stdin hub. Dropping it releases
/// the hub slot so a later consumer can receive chunks. A `dead`
/// subscription is one whose reader thread could not be started: it
/// reports EOF forever instead of parking (see [subscribe]).
pub(crate) struct StdinSub {
    id: u64,
    dead: bool,
}

impl StdinSub {
    /// Blocks until a chunk (or EOF) is available for this subscription.
    pub(crate) fn wait_take(&self) -> Option<Vec<u8>> {
        if self.dead {
            return None;
        }
        HUB.wait_take(self.id)
    }

    fn try_take(&self) -> Option<Option<Vec<u8>>> {
        if self.dead {
            return Some(None);
        }
        HUB.try_take(self.id)
    }
}

impl Drop for StdinSub {
    fn drop(&mut self) {
        HUB.unsubscribe(self.id);
    }
}

/// Hub state. Every method runs under the hub mutex, so the logic is plain
/// and unit-testable without threads.
struct HubState {
    buf: VecDeque<Vec<u8>>,
    /// Total buffered bytes, for the high-water mark.
    bytes: usize,
    /// Sticky: once process stdin hit EOF, every future subscriber sees it.
    eof: bool,
    /// Subscriber queue; the front id is the consumer currently entitled to
    /// chunks.
    subs: VecDeque<u64>,
    /// Event-loop wakes by subscriber id, signalled when the front
    /// subscriber gains data (push, EOF, or becoming front).
    wakers: HashMap<u64, Arc<LoopWake>>,
    next_id: u64,
    started: bool,
}

impl HubState {
    fn new() -> Self {
        HubState {
            buf: VecDeque::new(),
            bytes: 0,
            eof: false,
            subs: VecDeque::new(),
            wakers: HashMap::new(),
            next_id: 1,
            started: false,
        }
    }

    fn push_chunk(&mut self, chunk: Vec<u8>) {
        self.bytes += chunk.len();
        self.buf.push_back(chunk);
    }

    /// Returns bytes a consumer pulled but never used to the front of the
    /// buffer; the next take sees them first, in order. May push the total
    /// past the high-water mark: the reader thread then blocks on its next
    /// push until the bytes are pulled (the overshoot is one consumer's
    /// leftover, bounded by what it took).
    fn push_front(&mut self, chunk: Vec<u8>) {
        self.bytes += chunk.len();
        self.buf.push_front(chunk);
    }

    fn take_chunk(&mut self) -> Option<Vec<u8>> {
        let chunk = self.buf.pop_front()?;
        self.bytes -= chunk.len();
        Some(chunk)
    }

    fn would_block(&self) -> bool {
        self.bytes >= HIGH_WATER_MARK
    }

    /// Pulls one event for `id`: `Some(Some(chunk))` is data, `Some(None)`
    /// is EOF (and retires the subscription), `None` means nothing to take
    /// right now - the buffer is empty and stdin is not at EOF, or another
    /// consumer is ahead in the queue. EOF is sticky for the consumer too: an
    /// id retired by its EOF (or unsubscribed) keeps seeing EOF, because it
    /// can never reach the front again and would otherwise block forever.
    fn take_event(&mut self, id: u64) -> Option<Option<Vec<u8>>> {
        if self.subs.front() != Some(&id) {
            if self.eof && !self.subs.contains(&id) {
                return Some(None);
            }
            return None;
        }
        if let Some(chunk) = self.take_chunk() {
            return Some(Some(chunk));
        }
        if self.eof {
            self.subs.pop_front();
            return Some(None);
        }
        None
    }
}

/// The hub: shared state plus one condvar for data, space, and front-of-
/// queue changes.
struct Hub {
    state: Mutex<HubState>,
    wake: Condvar,
}

impl Hub {
    fn new() -> Self {
        Hub {
            state: Mutex::new(HubState::new()),
            wake: Condvar::new(),
        }
    }

    /// The front subscriber's loop wake, if it registered one.
    fn front_wake(st: &HubState) -> Option<Arc<LoopWake>> {
        st.subs.front().and_then(|id| st.wakers.get(id)).cloned()
    }

    /// Parks a chunk, blocking the reader while the buffer is at the
    /// high-water mark.
    fn push(&self, chunk: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        while st.would_block() {
            st = self.wake.wait(st).unwrap();
        }
        st.push_chunk(chunk);
        let front = Self::front_wake(&st);
        drop(st);
        self.wake.notify_all();
        if let Some(w) = front {
            w.signal();
        }
    }

    /// Marks process stdin at EOF (sticky).
    fn close(&self) {
        let front = {
            let mut st = self.state.lock().unwrap();
            st.eof = true;
            Self::front_wake(&st)
        };
        self.wake.notify_all();
        if let Some(w) = front {
            w.signal();
        }
    }

    /// Returns leftover bytes to the front of the buffer, for a session
    /// that ended before it could read them ("input a run does not read
    /// is kept").
    fn push_front(&self, chunk: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        st.push_front(chunk);
        let front = Self::front_wake(&st);
        drop(st);
        self.wake.notify_all();
        if let Some(w) = front {
            w.signal();
        }
    }

    /// Lets a later subscribe retry the reader: called only when the spawn
    /// failed, so `started` keeps meaning "a reader thread exists".
    fn spawn_failed(&self) {
        self.state.lock().unwrap().started = false;
        self.wake.notify_all();
    }

    /// Enqueues a subscriber; the bool says whether the reader thread still
    /// needs starting (decided under the lock, so it happens once).
    #[cfg(test)]
    fn subscribe(&self) -> (u64, bool) {
        self.subscribe_wake(None)
    }

    fn subscribe_wake(&self, wake: Option<Arc<LoopWake>>) -> (u64, bool) {
        let mut st = self.state.lock().unwrap();
        let id = st.next_id;
        st.next_id += 1;
        st.subs.push_back(id);
        if let Some(w) = wake {
            st.wakers.insert(id, w);
        }
        let start = !st.started;
        st.started = true;
        drop(st);
        self.wake.notify_all();
        (id, start)
    }

    fn unsubscribe(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if let Some(pos) = st.subs.iter().position(|sid| *sid == id) {
            st.subs.remove(pos);
        }
        st.wakers.remove(&id);
        // A new front may now have buffered chunks waiting for it.
        let front = Self::front_wake(&st);
        drop(st);
        self.wake.notify_all();
        if let Some(w) = front {
            w.signal();
        }
    }

    fn try_take(&self, id: u64) -> Option<Option<Vec<u8>>> {
        let ev = self.state.lock().unwrap().take_event(id);
        self.wake.notify_all();
        ev
    }

    /// Blocks until an event (chunk or EOF) exists for `id`.
    fn wait_take(&self, id: u64) -> Option<Vec<u8>> {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(ev) = st.take_event(id) {
                self.wake.notify_all();
                return ev;
            }
            st = self.wake.wait(st).unwrap();
        }
    }
}

static HUB: LazyLock<Hub> = LazyLock::new(Hub::new);

/// Subscribes (and starts the one reader thread on first use). A failed
/// spawn must not panic (subscribe runs inside V8 callbacks, where a panic
/// aborts the process): that subscription reports EOF and the next
/// subscribe retries the spawn.
pub(crate) fn subscribe() -> StdinSub {
    subscribe_inner(None)
}

/// Like [subscribe], but the hub signals `wake` when this subscription
/// (as front of the queue) gains a chunk, EOF, or the front itself —
/// how a run's event loop learns to stop parking.
pub(crate) fn subscribe_with_wake(wake: Arc<LoopWake>) -> StdinSub {
    subscribe_inner(Some(wake))
}

fn subscribe_inner(wake: Option<Arc<LoopWake>>) -> StdinSub {
    let (id, start_reader) = HUB.subscribe_wake(wake);
    if start_reader {
        let spawned = std::thread::Builder::new()
            .name("tardigrade-stdin".to_string())
            .spawn(reader_loop);
        if let Err(e) = spawned {
            eprintln!("tardigrade: cannot spawn stdin reader: {}", e);
            HUB.unsubscribe(id);
            HUB.spawn_failed();
            return StdinSub { id, dead: true };
        }
    }
    StdinSub { id, dead: false }
}

/// Returns bytes to the hub (see [Hub::push_front]).
pub(crate) fn push_front(chunk: Vec<u8>) {
    HUB.push_front(chunk);
}

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
        let n = match stdin.read(&mut buf) {
            Ok(n) => n,
            // A interrupted syscall is retryable, not EOF and not a real
            // error; everything else ends the stream as before.
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        if n == 0 {
            if !pending.is_empty() {
                HUB.push(std::mem::take(&mut pending));
            }
            HUB.close();
            return;
        }
        pending.extend_from_slice(&buf[..n]);
        let ready_len = complete_utf8_len(&pending);
        if ready_len == 0 {
            continue;
        }
        let rest = pending.split_off(ready_len);
        let ready = std::mem::replace(&mut pending, rest);
        HUB.push(ready);
    }
}

guest_cb!(stdin_read, |_args, rv, scope| {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    {
        let world = world_mut(scope);
        if !world.stdin_eof && world.stdin_sub.is_none() {
            let wake = world.loop_wake.clone();
            world.stdin_sub = Some(subscribe_with_wake(wake));
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
fn resolve_stdin_waiters(scope: &mut v8::PinScope<'_, '_>, chunk: Option<&[u8]>) {
    if let Some(bytes) = chunk {
        let resolver = world_mut(scope).stdin_waiters.pop();
        if let Some(resolver) = resolver {
            // SAFETY: the resolver global belongs to this isolate.
            let r = unsafe { resolver.open(scope) };
            let s = v8::String::new_from_utf8(scope, bytes, v8::NewStringType::Normal).unwrap();
            r.resolve(scope, s.into());
        }
        return;
    }
    let waiters: Vec<v8::Global<v8::PromiseResolver>> =
        std::mem::take(&mut world_mut(scope).stdin_waiters);
    let empty = v8::String::new(scope, "").unwrap();
    for w in waiters {
        // SAFETY: the waiter global belongs to this isolate.
        unsafe { w.open(scope) }.resolve(scope, empty.into());
    }
}

/// A subscription is held only while readers are pending; otherwise one
/// long-lived reader would starve every later subscriber.
fn release_idle_subscription(scope: &mut v8::PinScope<'_, '_>) {
    world_mut(scope).stdin_sub = None; // Drop unsubscribes from the hub
}

/// Delivers one hub event (if any) to the waiting readers. True when a
/// waiter was resolved: its continuations have run or are still queued,
/// and owe the event loop a checkpoint check.
pub(crate) fn poll_stdin(scope: &mut v8::PinScope<'_, '_>) -> bool {
    if world_mut(scope).stdin_waiters.is_empty() {
        release_idle_subscription(scope);
        return false;
    }
    if world_mut(scope).stdin_eof {
        resolve_stdin_waiters(scope, None);
        return true;
    }
    let event = world_mut(scope)
        .stdin_sub
        .as_ref()
        .and_then(|sub| sub.try_take());
    let delivered = match event {
        Some(Some(bytes)) => {
            resolve_stdin_waiters(scope, Some(&bytes));
            true
        }
        Some(None) => {
            world_mut(scope).stdin_eof = true;
            resolve_stdin_waiters(scope, None);
            true
        }
        None => false,
    };
    // A subscription is held only while readers are pending; otherwise one
    // long-lived reader would starve every later subscriber.
    if world_mut(scope).stdin_waiters.is_empty() {
        release_idle_subscription(scope);
    }
    delivered
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
            assert_eq!(
                complete_utf8_len(
                    &b"xy"
                        .iter()
                        .chain(&four[..cut])
                        .copied()
                        .collect::<Vec<_>>()
                ),
                2
            );
        }
    }

    #[test]
    fn utf8_len_does_not_hold_back_invalid_bytes() {
        assert_eq!(complete_utf8_len(&[0xFF]), 1);
        assert_eq!(complete_utf8_len(&[b'a', 0x80, 0x80]), 3);
        assert_eq!(complete_utf8_len(&[0xC0]), 1);
    }

    #[test]
    fn hub_eof_is_sticky_for_later_subscribers() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        h.push(b"x".to_vec());
        h.close();
        assert_eq!(h.try_take(a), Some(Some(b"x".to_vec())));
        assert_eq!(h.try_take(a), Some(None));
        let (b, _) = h.subscribe();
        assert_eq!(h.try_take(b), Some(None));
    }

    #[test]
    fn hub_eof_stays_visible_to_the_consumer_it_retired() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        h.close();
        assert_eq!(h.try_take(a), Some(None));
        assert_eq!(
            h.try_take(a),
            Some(None),
            "a second pull must not look blocked"
        );
    }

    /// Runs `f` on a thread and fails instead of hanging if it takes over 5 s.
    fn finishes_soon<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("hub call blocked instead of returning")
    }

    #[test]
    fn hub_wait_take_after_the_final_chunk_and_eof_does_not_block() {
        // The serve shape: the last command has no newline, so the consumer
        // takes the chunk, then EOF, then asks again for the next command.
        let h = std::sync::Arc::new(Hub::new());
        let (a, _) = h.subscribe();
        h.push(b"1+1".to_vec());
        h.close();
        let h2 = h.clone();
        let events = finishes_soon(move || (h2.wait_take(a), h2.wait_take(a), h2.wait_take(a)));
        assert_eq!(events, (Some(b"1+1".to_vec()), None, None));
    }

    #[test]
    fn hub_close_wakes_a_consumer_parked_in_wait_take() {
        let h = std::sync::Arc::new(Hub::new());
        let (a, _) = h.subscribe();
        let (tx, rx) = std::sync::mpsc::channel();
        let consumer = {
            let h = h.clone();
            std::thread::spawn(move || {
                let _ = tx.send(h.wait_take(a));
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.close();
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("close() must wake the parked consumer");
        assert_eq!(got, None);
        consumer.join().unwrap();
    }

    #[test]
    fn hub_drains_buffered_chunks_before_eof() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        h.push(b"one".to_vec());
        h.push(b"two".to_vec());
        h.close();
        assert_eq!(h.try_take(a), Some(Some(b"one".to_vec())));
        assert_eq!(h.try_take(a), Some(Some(b"two".to_vec())));
        assert_eq!(h.try_take(a), Some(None));
    }

    #[test]
    fn hub_keeps_buffered_chunks_across_unsubscribe() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        h.push(b"kept".to_vec());
        h.unsubscribe(a);
        let (b, _) = h.subscribe();
        assert_eq!(h.try_take(b), Some(Some(b"kept".to_vec())));
    }

    #[test]
    fn hub_unsubscribe_rotates_the_front_subscriber() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        let (b, _) = h.subscribe();
        h.push(b"m".to_vec());
        assert_eq!(h.try_take(b), None, "a is ahead in the queue");
        h.unsubscribe(a);
        assert_eq!(h.try_take(b), Some(Some(b"m".to_vec())));
    }

    #[test]
    fn hub_push_blocks_past_the_high_water_mark_until_a_pull() {
        let h = std::sync::Arc::new(Hub::new());
        let (a, _) = h.subscribe();
        h.push(vec![b'x'; HIGH_WATER_MARK]);
        let feeder = {
            let h = h.clone();
            std::thread::spawn(move || h.push(b"over".to_vec()))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !feeder.is_finished(),
            "push past the high-water mark must block"
        );
        assert_eq!(h.try_take(a), Some(Some(vec![b'x'; HIGH_WATER_MARK])));
        feeder.join().unwrap();
        assert_eq!(h.try_take(a), Some(Some(b"over".to_vec())));
    }

    #[test]
    fn hub_push_front_returns_bytes_to_the_next_take_in_order() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        h.push(b"one".to_vec());
        assert_eq!(h.try_take(a), Some(Some(b"one".to_vec())));
        h.push_front(b"back".to_vec());
        h.push(b"two".to_vec());
        assert_eq!(h.try_take(a), Some(Some(b"back".to_vec())));
        assert_eq!(h.try_take(a), Some(Some(b"two".to_vec())));
    }

    #[test]
    fn hub_push_front_survives_a_close_and_reaches_a_later_subscriber() {
        let h = Hub::new();
        let (a, _) = h.subscribe();
        h.push(b"kept".to_vec());
        h.close();
        h.unsubscribe(a);
        h.push_front(b"returned".to_vec());
        let (b, _) = h.subscribe();
        assert_eq!(h.try_take(b), Some(Some(b"returned".to_vec())));
        assert_eq!(h.try_take(b), Some(Some(b"kept".to_vec())));
        assert_eq!(h.try_take(b), Some(None));
    }

    #[test]
    fn a_failed_reader_spawn_can_be_retried() {
        let h = Hub::new();
        let (_, start) = h.subscribe();
        assert!(start);
        h.spawn_failed();
        let (_, start_again) = h.subscribe();
        assert!(
            start_again,
            "started must reflect a live reader, not an attempt"
        );
    }

    #[test]
    fn a_dead_subscription_reports_eof_and_never_blocks() {
        let sub = StdinSub {
            id: 4242,
            dead: true,
        };
        assert_eq!(sub.try_take(), Some(None));
        assert_eq!(sub.wait_take(), None);
    }
}

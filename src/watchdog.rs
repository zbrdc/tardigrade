//! Wall-clock watchdog and termination deliverer: safe thread-safe handle,
//! no raw pointers.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::world::{TermReason, Termination};

/// How often the thread wakes to check for a marked-but-undelivered
/// termination. A termination raised inside a GC callback (the heap
/// epilogue) cannot be delivered from there - V8 defers and loses stack
/// guard requests made during a collection - so this thread, which is
/// always outside any GC, re-requests it until the run ends.
const DELIVER_SLICE: Duration = Duration::from_millis(2);

pub(crate) struct Watchdog {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    /// IsolateHandle is Clone + Send + Sync, so it moves into the watchdog
    /// thread without any unsafe Send wrapper. The thread always runs (even
    /// with the timeout disabled): besides the wall-clock deadline it
    /// delivers marked terminations, because terminate_execution() is
    /// documented as callable from any thread - and unlike a GC callback,
    /// this thread is never inside the isolate when it calls.
    pub(crate) fn new(iso: v8::IsolateHandle, timeout_ms: u64, term: Arc<Termination>) -> Self {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let thread = {
            let state2 = state.clone();
            std::thread::spawn(move || {
                let (lock, cv) = &*state2;
                let deadline = if timeout_ms == 0 {
                    None
                } else {
                    Some(Duration::from_millis(timeout_ms))
                };
                let mut waited = Duration::ZERO;
                let stopped = lock.lock().unwrap();
                let mut guard = stopped;
                loop {
                    // Predicate-aware wait in short slices: an early wake
                    // means the run ended; otherwise each slice is both a
                    // deadline step and a chance to deliver a termination
                    // marked by the heap callbacks.
                    let slice = deadline.map_or(DELIVER_SLICE, |d| d.min(DELIVER_SLICE));
                    let (g, result) = cv.wait_timeout_while(guard, slice, |s| !*s).unwrap();
                    guard = g;
                    if !result.timed_out() {
                        return; // woken by Drop's stop signal
                    }
                    waited += slice;
                    if deadline.map(|d| waited >= d).unwrap_or(false) {
                        term.mark(TermReason::Timeout);
                        iso.terminate_execution();
                        return;
                    }
                    if term.reason().is_some() {
                        iso.terminate_execution();
                    }
                }
            })
        };
        Watchdog {
            stop: state,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        *self.stop.0.lock().unwrap() = true;
        self.stop.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

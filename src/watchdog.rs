//! Wall-clock watchdog and termination deliverer: safe thread-safe handle,
//! no raw pointers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::world::{TermReason, Termination, Wake};

/// How often the deliverer re-requests a marked termination once one
/// exists. Only runs after a reason is set, so an idle guest's thread
/// sleeps until its deadline untouched.
const DELIVER_SLICE: Duration = Duration::from_millis(2);

pub(crate) struct Watchdog {
    term: Arc<Termination>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    /// IsolateHandle is Clone + Send + Sync, so it moves into the watchdog
    /// thread without any unsafe Send wrapper. The thread blocks on the
    /// shared termination's condvar until the wall-clock deadline or until
    /// woken by a marked reason; only then does it poll, re-requesting the
    /// termination (a request raised inside a V8 callback can be deferred
    /// and lost, so an outside-the-isolate re-request is the reliable
    /// delivery).
    pub(crate) fn new(iso: v8::IsolateHandle, timeout_ms: u64, term: Arc<Termination>) -> Self {
        let term2 = term.clone();
        let thread = std::thread::spawn(move || {
            let deadline = if timeout_ms == 0 {
                None
            } else {
                Some(Instant::now() + Duration::from_millis(timeout_ms))
            };
            match term2.wait_for(deadline) {
                Wake::Stopped => {}
                Wake::Deadline => {
                    term2.mark(TermReason::Timeout);
                    iso.terminate_execution();
                }
                // Deliver: keep re-requesting until the run ends, so a
                // request lost inside a V8 callback gets retried from
                // outside the isolate.
                Wake::Reason => {
                    while !term2.is_finished() {
                        iso.terminate_execution();
                        std::thread::sleep(DELIVER_SLICE);
                    }
                }
            }
        });
        Watchdog { term, thread: Some(thread) }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // Concluding the run also wakes (and ends) the waiter; the thread
        // exits its delivery loop on the same flag.
        self.term.finish();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

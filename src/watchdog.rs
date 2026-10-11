//! Wall-clock watchdog: a safe thread-safe isolate handle, no raw pointers.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) struct Watchdog {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    /// IsolateHandle is Clone + Send + Sync, so it moves into the watchdog
    /// thread without any unsafe Send wrapper. terminate_execution() is
    /// documented as callable from any thread and returns false once the
    /// isolate is gone.
    pub(crate) fn new(iso: v8::IsolateHandle, timeout_ms: u64) -> Self {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let thread = if timeout_ms == 0 {
            None
        } else {
            let state2 = state.clone();
            Some(std::thread::spawn(move || {
                let (lock, cv) = &*state2;
                let stopped = lock.lock().unwrap();
                // Predicate-aware wait: handles notify-before-wait and
                // spurious wakeups, unlike a bare wait_timeout.
                let (_guard, result) = cv
                    .wait_timeout_while(stopped, Duration::from_millis(timeout_ms), |s| !*s)
                    .unwrap();
                if result.timed_out() {
                    iso.terminate_execution();
                }
            }))
        };
        Watchdog {
            stop: state,
            thread,
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

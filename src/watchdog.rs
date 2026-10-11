//! Wall-clock watchdogs and termination deliverer: safe thread-safe
//! handle, no raw pointers.
//!
//! Two watchdogs exist. The run watchdog enforces --timeout and delivers
//! outside terminations (the heap cap) until the run finishes. The
//! command watchdog enforces --command-timeout inside a serve command;
//! its firing is recovered by serve_loop and the session continues, so it
//! must be the sole owner of its termination (mark, deliver, then wait
//! for the host's ack before going quiet).

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::world::{TermReason, Termination, Wake};

/// How often a deliverer re-requests a marked termination once one
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
        let spawned = std::thread::Builder::new()
            .name("tardigrade-run-wd".to_string())
            .spawn(move || {
                let deadline = if timeout_ms == 0 {
                    None
                } else {
                    Some(Instant::now() + Duration::from_millis(timeout_ms))
                };
                loop {
                    match term2.wait_for(deadline) {
                        Wake::Finished => return,
                        Wake::Deadline => {
                            if term2.mark(TermReason::Timeout) {
                                iso.terminate_execution();
                                return;
                            }
                            // A command timeout holds the reason and is about
                            // to be recovered by serve_loop; re-wait and fire
                            // again once it is cleared (the deadline, being
                            // past, fires immediately).
                            std::thread::sleep(DELIVER_SLICE);
                        }
                        Wake::Reason => {
                            // Deliver a run-level reason, re-checking it every
                            // slice: serve_loop consumes CommandTimeout reasons
                            // to recover a session, and a reason read as None
                            // (consumed between the wake and here) is nobody's
                            // to deliver - terminating for it would kill a
                            // healthy session with a bare 124.
                            let run_level = |t: &Termination| {
                                matches!(t.reason(), Some(r) if r != TermReason::CommandTimeout)
                            };
                            if !run_level(&term2) {
                                // Nothing to deliver (a command timeout being
                                // recovered, or a reason consumed meanwhile):
                                // yield a slice. wait_for would return Reason
                                // again at once while the flag holds - a busy
                                // spin for as long as the guest sits in an
                                // interrupt-blind builtin.
                                std::thread::sleep(DELIVER_SLICE);
                                continue;
                            }
                            while !term2.is_finished() && run_level(&term2) {
                                iso.terminate_execution();
                                std::thread::sleep(DELIVER_SLICE);
                            }
                            // The reason was consumed: the run's outcome is
                            // decided, so fall through and wait (for the
                            // deadline or finish), not deliver.
                        }
                    }
                }
            });
        let thread = match spawned {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("tardigrade: cannot spawn run watchdog: {}", e);
                None
            }
        };
        Watchdog { term, thread }
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

/// The per-command deadline of one serve command. Unlike the run
/// watchdog, a firing does not conclude anything by itself: the command's
/// JS is terminated so the guest stops, and serve_loop - the only reader
/// of TermReason::CommandTimeout - recovers (settle, cancel, clear) and
/// the session continues with its state.
pub(crate) struct CommandWatchdog {
    /// The spawned thread and the switch that ends it; None when no
    /// timeout is configured (the watchdog is a no-op).
    guard: Option<Guard>,
}

struct Guard {
    /// Ends the thread: set once the command is over (normally or by
    /// recovery); the thread exits its park or its re-request loop on it.
    stop: Arc<(Mutex<bool>, Condvar)>,
    /// The spawned thread, when the spawn worked; None leaves the command
    /// without a deadline (a spawn failure must not panic here - it is the
    /// same failure it would be guarding).
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CommandWatchdog {
    /// A no-op watchdog when `deadline` is None.
    pub(crate) fn start(
        iso: v8::IsolateHandle,
        term: Arc<Termination>,
        deadline: Option<Instant>,
    ) -> Self {
        CommandWatchdog {
            guard: deadline.map(|d| spawn_guard(iso, term, d)),
        }
    }

    /// Ends the thread and waits for it: after settle() returns, no
    /// terminate_execution can arrive from it any more. The recovery
    /// needs exactly that ordering - join before cancelling V8's flag, or
    /// the last re-request would re-arm the flag right after the cancel
    /// (the known poisoning failure mode). Idempotent; runs on drop too.
    pub(crate) fn settle(&mut self) {
        if let Some(g) = self.guard.take() {
            *g.stop.0.lock().unwrap() = true;
            g.stop.1.notify_all();
            if let Some(t) = g.thread {
                let _ = t.join();
            }
        }
    }
}

impl Drop for CommandWatchdog {
    fn drop(&mut self) {
        self.settle();
    }
}

/// A spawn failure only disables the deadline for one command (like
/// a run without --command-timeout); the session itself is unaffected.
fn spawn_guard(iso: v8::IsolateHandle, term: Arc<Termination>, deadline: Instant) -> Guard {
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let parked = stop.clone();
    let thread = std::thread::Builder::new()
        .name("tardigrade-command-watchdog".to_string())
        .spawn(move || command_thread(iso, term, parked, deadline));
    match thread {
        Ok(t) => Guard {
            stop,
            thread: Some(t),
        },
        Err(e) => {
            eprintln!("tardigrade: cannot spawn command watchdog: {}", e);
            Guard { stop, thread: None }
        }
    }
}

/// Parks until `deadline` (false) or a stop (true). Pure condvar state,
/// unit-testable without an isolate.
fn park_until(stop: &(Mutex<bool>, Condvar), deadline: Instant) -> bool {
    let mut g = stop.0.lock().unwrap();
    while !*g {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        // wait_timeout_while, not wait_timeout: a stop that happened
        // before the wait must still wake us, or the host's settle()
        // would block for the whole remaining budget.
        let (g2, _) = stop
            .1
            .wait_timeout_while(g, deadline - now, |stopped| !*stopped)
            .unwrap();
        g = g2;
    }
    true
}

/// Only if its mark wins the first-wins race does it mark
/// CommandTimeout and keep re-requesting the termination until the host
/// acknowledges; a run-level reason that won instead belongs to the run
/// watchdog.
fn command_thread(
    iso: v8::IsolateHandle,
    term: Arc<Termination>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    deadline: Instant,
) {
    if park_until(&stop, deadline) {
        return;
    }
    if !term.mark(TermReason::CommandTimeout) {
        return;
    }
    iso.terminate_execution();
    loop {
        if *stop.0.lock().unwrap() {
            return;
        }
        iso.terminate_execution();
        std::thread::sleep(DELIVER_SLICE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Runs `f` on a thread and fails instead of hanging if it takes over
    /// 5 s, so a lost wakeup fails the test instead of hanging it.
    fn finishes_soon<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let done = Arc::new(AtomicBool::new(false));
        let mark = done.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = f();
            mark.store(true, Ordering::SeqCst);
            let _ = tx.send(out);
        });
        let bound = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < bound, "watchdog call blocked instead of returning");
            std::thread::sleep(Duration::from_millis(5));
        }
        rx.recv().expect("result after done")
    }

    #[test]
    fn park_until_wakes_from_a_stop_before_the_deadline() {
        // A far deadline means only the stop can end the park; the bound
        // proves the park woke instead of sleeping the budget away.
        let far = Instant::now() + Duration::from_secs(60);
        let flag = Arc::new((Mutex::new(false), Condvar::new()));
        let flag2 = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            *flag2.0.lock().unwrap() = true;
            flag2.1.notify_all();
        });
        assert!(finishes_soon(move || park_until(&flag, far)));
    }

    #[test]
    fn park_until_honors_a_stop_made_before_the_wait() {
        // Already stopped: the predicate fails at once, no wait at all.
        let stop = (Mutex::new(true), Condvar::new());
        let far = Instant::now() + Duration::from_secs(60);
        assert!(park_until(&stop, far));
    }

    #[test]
    fn park_until_returns_false_at_a_past_deadline() {
        let stop = (Mutex::new(false), Condvar::new());
        assert!(!park_until(&stop, Instant::now()));
    }
}

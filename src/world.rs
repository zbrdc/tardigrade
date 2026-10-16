//! Per-run guest state, stored in the isolate so several guests can run in
//! one process without seeing each other.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::clock::RunClock;
use crate::fs::RamFs;
use crate::heap::AllocState;
use crate::stdin::StdinSub;
use crate::timers::RunningTimer;

/// A scheduled guest timer. The callback is a persistent handle so it
/// survives between loop turns. The deadline sits on the run's clock
/// timeline in its own ticks (nanoseconds under Real, milliseconds
/// under Virtual), so real and virtual runs share one scheduling path;
/// `seq` is the registration order, and timers due at the same instant
/// fire in it (Node and mainline are FIFO, and equal deadlines are
/// common in virtual mode's whole milliseconds).
pub(crate) struct Timer {
    pub(crate) id: u64,
    pub(crate) seq: u64,
    pub(crate) repeat: bool,
    pub(crate) interval_ms: u64,
    pub(crate) when: u64,
    pub(crate) cb: v8::Global<v8::Function>,
}

/// One queued in-memory network message. The payload bytes are host
/// memory the guest never sees as an ArrayBuffer, so they are charged to
/// the ArrayBuffer budget for as long as the message sits in the queue;
/// dropping the message (delivery or run teardown) returns them.
pub(crate) struct NetMsg {
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) payload: String,
    pub(crate) budget: Arc<AllocState>,
}

impl Drop for NetMsg {
    fn drop(&mut self) {
        self.budget.release(self.payload.len());
    }
}

/// A rejection V8 reported without a handler, kept until the next microtask
/// checkpoint decides whether the run fails (Node semantics: attaching a
/// handler late in the same tick, or in a microtask, is not a failure).
#[derive(Clone)]
pub(crate) struct PendingRejection {
    /// Insertion order, so the first rejection can still be found across
    /// the identity-hash buckets.
    pub(crate) seq: u64,
    pub(crate) promise: v8::Global<v8::Promise>,
    pub(crate) value: Option<v8::Global<v8::Value>>,
}

/// Cross-thread wake for the event loop: the stdin reader and network
/// worker threads signal it when work lands, so the loop's park is a
/// condvar wait instead of a poll. The generation counter makes the
/// wait race-free: a signal that lands between the loop's pending-work
/// check and its wait makes the wait return at once.
pub(crate) struct LoopWake {
    inner: (Mutex<u64>, Condvar),
}

impl LoopWake {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(LoopWake {
            inner: (Mutex::new(0), Condvar::new()),
        })
    }

    pub(crate) fn generation(&self) -> u64 {
        *self.inner.0.lock().unwrap()
    }

    pub(crate) fn signal(&self) {
        *self.inner.0.lock().unwrap() += 1;
        self.inner.1.notify_all();
    }

    /// Waits until the generation moves past `seen` or `dur` elapses.
    pub(crate) fn wait(&self, seen: u64, dur: std::time::Duration) {
        let guard = self.inner.0.lock().unwrap();
        if *guard != seen {
            return;
        }
        let _ = self.inner.1.wait_timeout(guard, dur).unwrap();
    }
}

/// Why a run was terminated from outside guest code. CommandTimeout is
/// serve-only: it is set by the per-command watchdog, recovered by
/// serve_loop, and never delivered as a run-level stop by the run
/// watchdog. RssUnavailable means a requested Linux RSS sample failed after
/// watchdog admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TermReason {
    Timeout,
    Oom,
    CommandTimeout,
    RssUnavailable,
}

/// Shared termination state: the watchdog thread and the heap callbacks
/// mark the reason before calling terminate_execution, and the JS thread
/// reads it at turn boundaries. V8's own termination flag can be swallowed
/// inside microtask checkpoints and GC callbacks, so the flag, not
/// is_execution_terminating(), is the reliable signal. `finished` silences
/// the heap callbacks for teardown GCs once the run has concluded.
pub(crate) struct Termination {
    reason: AtomicU8,
    finished: AtomicBool,
    /// Wakes the watchdog: blocked until the deadline, or until a mark or
    /// finish wakes it (a bare atomic cannot do that).
    wake: (Mutex<()>, Condvar),
}

/// What ended a [Termination::wait_for].
pub(crate) enum Wake {
    /// The run has finished; the waiter should exit.
    Finished,
    /// The deadline passed without a reason.
    Deadline,
    /// A termination reason is set and wants delivering.
    Reason,
}

/// A serve command timeout surfaces through the i32 channels as this
/// sentinel so serve_loop can tell it from every run-level outcome. It is
/// consumed before the session code is decided and can never escape as a
/// process exit code (a guest vm.exit(-2) can equal it, but every serve
/// path checks a recorded guest exit first, so the guest code wins).
pub(crate) const COMMAND_TIMEOUT_CODE: i32 = -2;

impl Termination {
    pub(crate) fn new() -> Self {
        Termination {
            reason: AtomicU8::new(0),
            finished: AtomicBool::new(false),
            wake: (Mutex::new(()), Condvar::new()),
        }
    }

    /// Marks why the run is being terminated; the first marker wins (an
    /// OOM found before a late watchdog firing still reports as OOM).
    /// True when this call set the reason, so a caller that lost the race
    /// knows another terminator owns the outcome. Re-marking the reason
    /// that already holds also wins: the marker agrees with the state.
    pub(crate) fn mark(&self, reason: TermReason) -> bool {
        let code = match reason {
            TermReason::Timeout => 1,
            TermReason::Oom => 2,
            TermReason::CommandTimeout => 3,
            TermReason::RssUnavailable => 4,
        };
        // Store and notify under the lock the waiter checks its predicate
        // under: without it, a mark between the waiter's check and its wait
        // is a lost wakeup.
        let _guard = self.wake.0.lock().unwrap();
        let won = self
            .reason
            .compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if won {
            self.wake.1.notify_all();
        }
        won
    }

    pub(crate) fn reason(&self) -> Option<TermReason> {
        match self.reason.load(Ordering::SeqCst) {
            1 => Some(TermReason::Timeout),
            2 => Some(TermReason::Oom),
            3 => Some(TermReason::CommandTimeout),
            4 => Some(TermReason::RssUnavailable),
            _ => None,
        }
    }

    /// Reads and clears the reason: exactly one caller concludes a run.
    pub(crate) fn take_reason(&self) -> Option<TermReason> {
        let reason = self.reason();
        self.reason.store(0, Ordering::SeqCst);
        reason
    }

    /// True once the run has concluded, so heap callbacks for teardown GCs
    /// must not mark or terminate any more.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub(crate) fn finish(&self) {
        // Under the waiter's lock, like [Termination::mark].
        let _guard = self.wake.0.lock().unwrap();
        self.finished.store(true, Ordering::SeqCst);
        self.wake.1.notify_all();
    }

    /// Blocks until the deadline (forever when None), waking early when a
    /// reason or finish lands. Spurious wakes re-evaluate and wait again.
    /// The mutex guard is dropped before returning, so callers may act on
    /// the result (e.g. terminate the isolate) without holding it.
    pub(crate) fn wait_for(&self, deadline: Option<Instant>) -> Wake {
        let mut guard = self.wake.0.lock().unwrap();
        loop {
            if self.finished.load(Ordering::SeqCst) {
                return Wake::Finished;
            }
            if self.reason().is_some() {
                return Wake::Reason;
            }
            match deadline.map(|d| d.saturating_duration_since(Instant::now())) {
                Some(Duration::ZERO) => return Wake::Deadline,
                wait => {
                    let (g, _) = self
                        .wake
                        .1
                        .wait_timeout(guard, wait.unwrap_or(Duration::MAX))
                        .unwrap();
                    guard = g;
                }
            }
        }
    }
}

/// All guest world state for one run. The isolate owns it through an embedder
/// slot (see [world_mut]); one JS thread touches it, and the watchdog thread
/// only ever calls Isolate::terminate_execution().
pub(crate) struct World {
    pub(crate) fs: RamFs,
    pub(crate) timers: Vec<Timer>,
    pub(crate) next_timer_id: u64,
    /// Monotonic registration counter: the FIFO tiebreak for timers that
    /// share a deadline. Reassigned on reschedule, so a repeating timer
    /// that collides with a fresh one queues behind it.
    pub(crate) next_timer_seq: u64,
    pub(crate) net: VecDeque<NetMsg>,
    pub(crate) listeners: BTreeMap<u64, v8::Global<v8::Function>>,
    pub(crate) next_port: u64,
    pub(crate) stdin_waiters: Vec<v8::Global<v8::PromiseResolver>>,
    pub(crate) stdin_eof: bool,
    /// True in serve mode: stdout is the reply channel there, so guest
    /// console output is diverted to stderr where it cannot be mistaken
    /// for (or desynchronise) a reply line.
    pub(crate) serve_mode: bool,
    /// Guest console output charged so far, and its cap (u64::MAX when
    /// disabled): a guest printing forever must not be able to fill a
    /// disk or a pipe the host is draining at unbounded volume.
    pub(crate) output_bytes: u64,
    pub(crate) output_cap: u64,
    /// This run's subscription to the process-wide stdin hub, created on the
    /// first vm.stdin.read() and released when the run ends - or sooner,
    /// whenever no vm.stdin readers are pending any more (see
    /// stdin::poll_stdin), so one long-held subscription cannot starve a
    /// later subscriber.
    pub(crate) stdin_sub: Option<StdinSub>,
    /// Math.random counter for seeded runs (Config.seed); None leaves
    /// V8's own Math.random in place.
    pub(crate) random_state: Option<u64>,
    /// The run's time source: real, or the virtual clock that advances
    /// only when the event loop jumps it to the next due timer.
    /// performance.now and the timer deadlines read it.
    pub(crate) clock: RunClock,
    /// The timer whose callback is on the stack, if any.
    pub(crate) running_timer: RunningTimer,
    /// The guest's vm.exit code; any i32, negatives included, is valid,
    /// but only once `exit_set` is true.
    exit_code: AtomicI32,
    exit_set: AtomicBool,
    /// Termination reason shared with the watchdog thread and the heap
    /// callbacks (they get no isolate reference, only this Arc).
    pub(crate) term: Arc<Termination>,
    /// The effective cap, for the OOM report.
    pub(crate) heap_mb: usize,
    /// The ArrayBuffer budget the counting allocator enforces; buffers the
    /// host builds from Rust bytes draw on it too, as do queued vm.net
    /// payloads (see [NetMsg]). Guest::new installs the run's real one;
    /// this default never refuses.
    pub(crate) ab_budget: Arc<AllocState>,
    /// Rejections awaiting the next microtask checkpoint, bucketed by the
    /// rejecting promise's identity hash so a mass catch does not scan
    /// every remembered rejection.
    pub(crate) pending_rejections: HashMap<i32, Vec<PendingRejection>>,
    pub(crate) next_rejection_seq: u64,
    /// Scoped network capabilities (netcap): granted names, limits, the
    /// wire fd to the sidecar, the completion queue its threads push into,
    /// and the pending resolvers the event loop resolves.
    pub(crate) net_limits: crate::netcap::NetLimits,
    pub(crate) net_names: Vec<String>,
    pub(crate) net_max_response: u64,
    pub(crate) net_total_bytes: u64,
    pub(crate) net_bytes_used: u64,
    pub(crate) net_fd: Option<std::os::fd::RawFd>,
    pub(crate) net_wire: Option<Arc<Mutex<std::os::unix::net::UnixStream>>>,
    pub(crate) net_done: crate::netcap::NetQueue,
    pub(crate) net_pending: HashMap<u64, v8::Global<v8::PromiseResolver>>,
    pub(crate) next_net_id: u64,
    /// ES module cache for module-mode runs: fs path -> module, and
    /// module identity hash -> fs path (so the import resolver knows
    /// who is asking). Empty for classic-script runs.
    pub(crate) modules: HashMap<String, v8::Global<v8::Module>>,
    pub(crate) module_paths: HashMap<i32, String>,
    /// Signalled by the stdin reader (through this run's subscription)
    /// and by network worker completions; the event loop waits on it.
    pub(crate) loop_wake: Arc<LoopWake>,
}

impl World {
    pub(crate) fn new(fs_quota_bytes: u64, clock: RunClock) -> Self {
        World {
            fs: RamFs::new(fs_quota_bytes),
            timers: Vec::new(),
            next_timer_id: 1,
            next_timer_seq: 0,
            net: VecDeque::new(),
            listeners: BTreeMap::new(),
            next_port: 1,
            stdin_waiters: Vec::new(),
            stdin_eof: false,
            serve_mode: false,
            output_bytes: 0,
            output_cap: u64::MAX,
            stdin_sub: None,
            random_state: None,
            clock,
            running_timer: RunningTimer::new(),
            exit_code: AtomicI32::new(-1),
            exit_set: AtomicBool::new(false),
            term: Arc::new(Termination::new()),
            heap_mb: 0,
            ab_budget: Arc::new(AllocState::new(usize::MAX)),
            pending_rejections: HashMap::new(),
            next_rejection_seq: 0,
            net_limits: Default::default(),
            net_names: Vec::new(),
            net_max_response: 0,
            net_total_bytes: 0,
            net_bytes_used: 0,
            net_fd: None,
            net_wire: None,
            net_done: Arc::new(Mutex::new(VecDeque::new())),
            net_pending: HashMap::new(),
            next_net_id: 1,
            modules: HashMap::new(),
            module_paths: HashMap::new(),
            loop_wake: LoopWake::new(),
        }
    }

    /// Concludes a terminated run: a guest vm.exit() code wins, then the
    /// heap cap (137, reported once on stderr), then RSS unavailability
    /// (70), then the watchdog (124).
    /// A command timeout maps to [COMMAND_TIMEOUT_CODE]: only serve_loop
    /// sees that value, and it recovers the session instead of ending it.
    /// Exactly one caller concludes the run; `take` makes a stray second
    /// call visibly wrong (124) instead of double-printing the report.
    pub(crate) fn take_terminated_exit(&mut self) -> i32 {
        let code = self.exit_code();
        if self.has_exit() {
            return code;
        }
        match self.term.take_reason() {
            Some(TermReason::Oom) => {
                eprintln!("tardigrade: heap limit exceeded ({} MB)", self.heap_mb);
                137
            }
            Some(TermReason::CommandTimeout) => COMMAND_TIMEOUT_CODE,
            Some(TermReason::RssUnavailable) => {
                eprintln!("tardigrade: cannot sample process RSS");
                70
            }
            _ => 124,
        }
    }

    /// True once the run has decided its outcome (a vm.exit code, or an
    /// outside termination) and guest callbacks must stop having effects.
    pub(crate) fn is_leaving(&self) -> bool {
        self.has_exit() || self.term.reason().is_some() || self.term.is_finished()
    }

    /// Concludes a run whose callback or script raised an uncaught
    /// exception: a guest exit code wins, then an outside termination
    /// (timeout, heap cap); only with neither is the exception itself the
    /// outcome. Callers report the exception only for the last case.
    pub(crate) fn exception_exit(&mut self) -> i32 {
        let code = self.exit_code();
        if self.has_exit() {
            return code;
        }
        if self.term.reason().is_some() {
            return self.take_terminated_exit();
        }
        1
    }

    /// True when neither an exit code nor a termination reason is set, so
    /// a pending exception is what decides the run (and should be
    /// reported).
    pub(crate) fn exception_decides(&self) -> bool {
        !self.has_exit() && self.term.reason().is_none()
    }

    pub(crate) fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::SeqCst)
    }

    /// True once vm.exit recorded a code. The code is checked through
    /// this, never through its value: negative codes are exits too.
    pub(crate) fn has_exit(&self) -> bool {
        self.exit_set.load(Ordering::SeqCst)
    }

    /// First exit wins: a later vm.exit during the termination window
    /// cannot change the code (its callback is blocked anyway, but be
    /// explicit about the contract).
    pub(crate) fn set_exit_code(&self, code: i32) {
        if self.exit_set.swap(true, Ordering::SeqCst) {
            return;
        }
        self.exit_code.store(code, Ordering::SeqCst);
    }
}
/// Reaches this run's World through the isolate embedder slot. Works from
/// any scope (HandleScope, TryCatch, CallbackScope...) because they all
/// expose the isolate. The returned reference borrows the scope, so unlike
/// the old process global it must not be held across another use of the
/// scope, e.g. running guest code: take it, use it, drop it.
pub(crate) fn world_mut<S: AsMut<v8::Isolate>>(scope: &mut S) -> &mut World {
    scope
        .as_mut()
        .get_slot_mut::<World>()
        .expect("world is installed in the isolate before callbacks can run")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs wait_for on its own thread and gives up after a few seconds, so
    /// a lost wakeup fails the test instead of hanging it. The thread is
    /// left behind on failure; the test process ends anyway.
    fn wait_bounded(term: &Arc<Termination>, deadline: Option<Instant>) -> Option<Wake> {
        let (tx, rx) = std::sync::mpsc::channel();
        let term = term.clone();
        std::thread::spawn(move || {
            let _ = tx.send(term.wait_for(deadline));
        });
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    #[test]
    fn wait_for_returns_deadline_when_the_deadline_has_passed() {
        let term = Arc::new(Termination::new());
        let past = Instant::now();
        assert!(matches!(
            wait_bounded(&term, Some(past)),
            Some(Wake::Deadline)
        ));
    }

    #[test]
    fn wait_for_returns_reason_for_a_mark_made_before_the_wait() {
        let term = Arc::new(Termination::new());
        term.mark(TermReason::Oom);
        let far = Instant::now() + Duration::from_secs(60);
        assert!(matches!(wait_bounded(&term, Some(far)), Some(Wake::Reason)));
    }

    #[test]
    fn finish_from_another_thread_unblocks_a_wait_without_a_deadline() {
        let term = Arc::new(Termination::new());
        let finisher = term.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            finisher.finish();
        });
        assert!(matches!(wait_bounded(&term, None), Some(Wake::Finished)));
        handle.join().unwrap();
    }

    #[test]
    fn finish_takes_precedence_over_a_reason() {
        let term = Arc::new(Termination::new());
        term.mark(TermReason::Timeout);
        term.finish();
        assert!(matches!(wait_bounded(&term, None), Some(Wake::Finished)));
    }

    #[test]
    fn mark_and_finish_publish_under_the_waiters_lock() {
        // A store-and-notify outside the lock can land between the
        // waiter's predicate check and its wait, and is then lost. While
        // the test holds the lock, neither may complete.
        let term = Arc::new(Termination::new());
        let held = term.wake.0.lock().unwrap();
        let marker = term.clone();
        let t1 = std::thread::spawn(move || marker.mark(TermReason::Timeout));
        let finisher = term.clone();
        let t2 = std::thread::spawn(move || finisher.finish());
        std::thread::sleep(Duration::from_millis(100));
        assert!(term.reason().is_none(), "mark must wait for the lock");
        assert!(!term.is_finished(), "finish must wait for the lock");
        drop(held);
        t1.join().unwrap();
        t2.join().unwrap();
        assert!(term.reason().is_some() && term.is_finished());
    }

    #[test]
    fn queued_messages_hold_their_budget_bytes_until_dropped() {
        let budget = Arc::new(crate::heap::AllocState::new(10));
        let msg = |len: usize| NetMsg {
            from: 1,
            to: 2,
            payload: "x".repeat(len),
            budget: budget.clone(),
        };
        // net_send's discipline: charge the payload before queueing it.
        assert!(budget.try_charge(4) && budget.try_charge(4));
        let queued = VecDeque::from([msg(4), msg(4)]);
        assert!(!budget.try_charge(3), "8 of 10 bytes are held");
        drop(queued);
        assert!(budget.try_charge(10), "dropping the queue returned all 8");
    }

    #[test]
    fn the_first_exit_code_wins() {
        let w = World::new(0, RunClock::Real(Instant::now()));
        w.set_exit_code(3);
        w.set_exit_code(4);
        assert_eq!(w.exit_code(), 3);
        let w2 = World::new(0, RunClock::Real(Instant::now()));
        w2.set_exit_code(0);
        w2.set_exit_code(9);
        assert_eq!(w2.exit_code(), 0, "an explicit 0 also wins");
    }

    #[test]
    fn a_negative_exit_code_is_an_exit() {
        let mut w = World::new(0, RunClock::Real(Instant::now()));
        w.set_exit_code(-1);
        assert!(w.has_exit());
        assert!(w.is_leaving());
        assert!(!w.exception_decides());
        assert_eq!(w.take_terminated_exit(), -1);
        w.set_exit_code(5);
        assert_eq!(w.exit_code(), -1, "the first exit still wins");
    }

    #[test]
    fn mark_reports_the_winner() {
        let term = Arc::new(Termination::new());
        assert!(term.mark(TermReason::Oom));
        assert!(!term.mark(TermReason::Timeout), "the first marker wins");
        assert_eq!(term.reason(), Some(TermReason::Oom));
    }

    #[test]
    fn a_command_timeout_maps_to_its_own_code_and_is_consumed() {
        let mut w = World::new(0, RunClock::Real(Instant::now()));
        w.term.mark(TermReason::CommandTimeout);
        assert_eq!(w.take_terminated_exit(), COMMAND_TIMEOUT_CODE);
        assert_eq!(w.term.reason(), None, "taken, like every reason");
        assert!(!w.is_leaving(), "the leaving guard fell with it");
    }

    #[test]
    fn unavailable_rss_maps_to_70_and_is_consumed_once() {
        let mut w = World::new(0, RunClock::Real(Instant::now()));
        assert!(w.term.mark(TermReason::RssUnavailable));
        assert_eq!(w.term.reason.load(Ordering::SeqCst), 4);
        assert_eq!(w.term.reason(), Some(TermReason::RssUnavailable));
        assert_eq!(w.take_terminated_exit(), 70);
        assert_eq!(w.term.reason(), None, "the reason is consumed");
        assert_eq!(w.take_terminated_exit(), 124, "no second report");
    }

    #[test]
    fn guest_exit_still_precedes_unavailable_rss() {
        let mut w = World::new(0, RunClock::Real(Instant::now()));
        assert!(w.term.mark(TermReason::RssUnavailable));
        w.set_exit_code(23);
        assert_eq!(w.take_terminated_exit(), 23);
        assert_eq!(
            w.term.reason(),
            Some(TermReason::RssUnavailable),
            "guest exit precedence does not consume the run-level reason"
        );
    }
}

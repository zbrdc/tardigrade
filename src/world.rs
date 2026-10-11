//! Per-run guest state, stored in the isolate so several guests can run in
//! one process without seeing each other.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::stdin::StdinSub;
use crate::timers::RunningTimer;

/// A scheduled guest timer. The callback is a persistent handle so it
/// survives between loop turns.
pub(crate) struct Timer {
    pub(crate) id: u64,
    pub(crate) repeat: bool,
    pub(crate) interval_ms: u64,
    pub(crate) when: Instant,
    pub(crate) cb: v8::Global<v8::Function>,
}

/// One queued in-memory network message.
pub(crate) struct NetMsg {
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) payload: String,
}

/// A rejection V8 reported without a handler, kept until the next microtask
/// checkpoint decides whether the run fails (Node semantics: attaching a
/// handler late in the same tick, or in a microtask, is not a failure).
pub(crate) struct PendingRejection {
    /// Insertion order, so the first rejection can still be found across
    /// the identity-hash buckets.
    pub(crate) seq: u64,
    pub(crate) promise: v8::Global<v8::Promise>,
    pub(crate) value: Option<v8::Global<v8::Value>>,
}

/// Why a run was terminated from outside guest code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TermReason {
    Timeout,
    Oom,
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
}

impl Termination {
    pub(crate) fn new() -> Self {
        Termination {
            reason: AtomicU8::new(0),
            finished: AtomicBool::new(false),
        }
    }

    /// Marks why the run is being terminated; the first marker wins (an
    /// OOM found before a late watchdog firing still reports as OOM).
    pub(crate) fn mark(&self, reason: TermReason) {
        let code = match reason {
            TermReason::Timeout => 1,
            TermReason::Oom => 2,
        };
        let _ = self.reason.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
    }

    pub(crate) fn reason(&self) -> Option<TermReason> {
        match self.reason.load(Ordering::SeqCst) {
            1 => Some(TermReason::Timeout),
            2 => Some(TermReason::Oom),
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
        self.finished.store(true, Ordering::SeqCst);
    }
}

/// All guest world state for one run. The isolate owns it through an embedder
/// slot (see [world_mut]); one JS thread touches it, and the watchdog thread
/// only ever calls Isolate::terminate_execution().
pub(crate) struct World {
    pub(crate) fs: BTreeMap<String, String>,
    pub(crate) timers: Vec<Timer>,
    pub(crate) next_timer_id: u64,
    pub(crate) net: VecDeque<NetMsg>,
    pub(crate) listeners: BTreeMap<u64, v8::Global<v8::Function>>,
    pub(crate) next_port: u64,
    pub(crate) stdin_waiters: Vec<v8::Global<v8::PromiseResolver>>,
    pub(crate) stdin_eof: bool,
    /// This run's subscription to the process-wide stdin hub, created on the
    /// first vm.stdin.read() and dropped (released) when the run ends.
    pub(crate) stdin_sub: Option<StdinSub>,
    /// The timer whose callback is on the stack, if any.
    pub(crate) running_timer: RunningTimer,
    exit_code: AtomicI32,
    /// Termination reason shared with the watchdog thread and the heap
    /// callbacks (they get no isolate reference, only this Arc).
    pub(crate) term: Arc<Termination>,
    /// The effective cap, for the OOM report.
    pub(crate) heap_mb: usize,
    /// Set while the end-of-run snapshot is evaluated: its guest-side
    /// helper only reads, so callbacks stay allowed for it.
    dumping: bool,
    /// Sticky: an OOM concluded this run (the reason itself is consumed by
    /// the single take that decides the exit code).
    pub(crate) heap_oom_happened: bool,
    /// Rejections awaiting the next microtask checkpoint, bucketed by the
    /// rejecting promise's identity hash so a mass catch does not scan
    /// every remembered rejection.
    pub(crate) pending_rejections: HashMap<i32, Vec<PendingRejection>>,
    pub(crate) next_rejection_seq: u64,
}

impl World {
    pub(crate) fn new() -> Self {
        World {
            fs: BTreeMap::new(),
            timers: Vec::new(),
            next_timer_id: 1,
            net: VecDeque::new(),
            listeners: BTreeMap::new(),
            next_port: 1,
            stdin_waiters: Vec::new(),
            stdin_eof: false,
            stdin_sub: None,
            running_timer: RunningTimer::new(),
            exit_code: AtomicI32::new(-1),
            term: Arc::new(Termination::new()),
            heap_mb: 0,
            dumping: false,
            heap_oom_happened: false,
            pending_rejections: HashMap::new(),
            next_rejection_seq: 0,
        }
    }

    /// Concludes a terminated run: a guest vm.exit() code wins, then the
    /// heap cap (137, reported once on stderr), then the watchdog (124).
    /// Exactly one caller concludes the run; `take` makes a stray second
    /// call visibly wrong (124) instead of double-printing the report.
    pub(crate) fn take_terminated_exit(&mut self) -> i32 {
        let code = self.exit_code();
        if code >= 0 {
            return code;
        }
        match self.term.take_reason() {
            Some(TermReason::Oom) => {
                eprintln!("v8vm: heap limit exceeded ({} MB)", self.heap_mb);
                self.heap_oom_happened = true;
                137
            }
            _ => 124,
        }
    }

    /// True once the run has decided its outcome (a vm.exit code, or an
    /// outside termination) and guest callbacks must stop having effects.
    pub(crate) fn is_leaving(&self) -> bool {
        !self.dumping
            && (self.exit_code() >= 0
                || self.term.reason().is_some()
                || self.term.is_finished())
    }

    /// Marks the end-of-run snapshot window (read-only callbacks allowed).
    pub(crate) fn set_dumping(&mut self, dumping: bool) {
        self.dumping = dumping;
    }

    /// Concludes a run whose callback or script raised an uncaught
    /// exception: a guest exit code wins, then an outside termination
    /// (timeout, heap cap); only with neither is the exception itself the
    /// outcome. Callers report the exception only for the last case.
    pub(crate) fn exception_exit(&mut self) -> i32 {
        let code = self.exit_code();
        if code >= 0 {
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
        self.exit_code() < 0 && self.term.reason().is_none()
    }

    pub(crate) fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::SeqCst)
    }

    /// First exit wins: a later vm.exit during the termination window
    /// cannot change the code (its callback is blocked anyway, but be
    /// explicit about the contract).
    pub(crate) fn set_exit_code(&self, code: i32) {
        let _ = self
            .exit_code
            .compare_exchange(-1, code, Ordering::SeqCst, Ordering::SeqCst);
    }}
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

    #[test]
    fn the_first_exit_code_wins() {
        let w = World::new();
        w.set_exit_code(3);
        w.set_exit_code(4);
        assert_eq!(w.exit_code(), 3);
        let w2 = World::new();
        w2.set_exit_code(0);
        w2.set_exit_code(9);
        assert_eq!(w2.exit_code(), 0, "an explicit 0 also wins");
    }
}

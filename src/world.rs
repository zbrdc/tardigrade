//! Per-run guest state, stored in the isolate so several guests can run in
//! one process without seeing each other.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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
    pub(crate) promise: v8::Global<v8::Promise>,
    pub(crate) value: Option<v8::Global<v8::Value>>,
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
    /// Set by V8's near-heap-limit callback (through the shared Arc it
    /// holds; the callback gets no isolate reference to reach this slot).
    pub(crate) heap_oom: Arc<AtomicBool>,
    /// The configured cap, for the OOM report.
    pub(crate) heap_mb: usize,
    /// Rejections awaiting the next microtask checkpoint; first one still
    /// unhandled when the checkpoint ends fails the run.
    pub(crate) pending_rejections: Vec<PendingRejection>,
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
            heap_oom: Arc::new(AtomicBool::new(false)),
            heap_mb: 0,
            pending_rejections: Vec::new(),
        }
    }

    /// Exit code for a cancelled execution termination: a guest vm.exit()
    /// code wins, then the heap cap (137, with a one-line report), then the
    /// wall-clock watchdog.
    pub(crate) fn terminated_exit(&self) -> i32 {
        let code = self.exit_code();
        if code >= 0 {
            return code;
        }
        if self.heap_oom.load(Ordering::SeqCst) {
            eprintln!("v8vm: heap limit exceeded ({} MB)", self.heap_mb);
            return 137;
        }
        124
    }

    pub(crate) fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::SeqCst)
    }

    pub(crate) fn set_exit_code(&self, code: i32) {
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

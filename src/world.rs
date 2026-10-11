//! Per-run guest state, stored in the isolate so several guests can run in
//! one process without seeing each other.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

use crate::fs::RamFs;
use crate::stdin::StdinSub;

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

/// All guest world state for one run. The isolate owns it through an embedder
/// slot (see [world_mut]); one JS thread touches it, and the watchdog thread
/// only ever calls Isolate::terminate_execution().
pub(crate) struct World {
    pub(crate) fs: RamFs,
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
    /// Math.random counter for seeded runs (Config.seed); None leaves
    /// V8's own Math.random in place.
    pub(crate) random_state: Option<u64>,
    /// Zero point of this run's performance.now(): the moment the run's
    /// state was created, i.e. the start of the run.
    pub(crate) perf_origin: std::time::Instant,
    // ids cleared via clear_timer while their callback was on the stack
    pub(crate) cleared: Vec<u64>,
    exit_code: AtomicI32,
    pub(crate) unhandled_rejection: Option<String>,
    pub(crate) unhandled_rejection_exc: Option<v8::Global<v8::Value>>,
}

impl World {
    pub(crate) fn new(fs_quota_bytes: u64) -> Self {
        World {
            fs: RamFs::new(fs_quota_bytes),
            timers: Vec::new(),
            next_timer_id: 1,
            net: VecDeque::new(),
            listeners: BTreeMap::new(),
            next_port: 1,
            stdin_waiters: Vec::new(),
            stdin_eof: false,
            stdin_sub: None,
            random_state: None,
            perf_origin: Instant::now(),
            cleared: Vec::new(),
            exit_code: AtomicI32::new(-1),
            unhandled_rejection: None,
            unhandled_rejection_exc: None,
        }
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

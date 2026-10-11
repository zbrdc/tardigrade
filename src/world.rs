//! Per-run guest state, stored in the isolate so several guests can run in
//! one process without seeing each other.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

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
    pub(crate) fs: BTreeMap<String, String>,
    pub(crate) timers: Vec<Timer>,
    pub(crate) next_timer_id: u64,
    pub(crate) net: VecDeque<NetMsg>,
    pub(crate) listeners: BTreeMap<u64, v8::Global<v8::Function>>,
    pub(crate) next_port: u64,
    pub(crate) stdin_waiters: Vec<v8::Global<v8::PromiseResolver>>,
    pub(crate) stdin_eof: bool,
    /// Shut-down signal for the reader thread; dropped with the world.
    pub(crate) stdin_thread: Option<Sender<Option<Vec<u8>>>>,
    /// Chunks produced by this run's reader thread (was a process global).
    pub(crate) stdin_chunks: Option<Receiver<Option<Vec<u8>>>>,
    // ids cleared via clear_timer while their callback was on the stack
    pub(crate) cleared: Vec<u64>,
    exit_code: AtomicI32,
    pub(crate) unhandled_rejection: Option<String>,
    pub(crate) unhandled_rejection_exc: Option<v8::Global<v8::Value>>,
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
            stdin_thread: None,
            stdin_chunks: None,
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

pub(crate) fn to_rust_string(scope: &mut v8::HandleScope, v: &v8::Value) -> String {
    match v.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => String::new(),
    }
}

pub(crate) fn throw(scope: &mut v8::HandleScope, msg: &str) {
    let msg = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc.into());
}

// ---- unhandled rejections ----

pub(crate) extern "C" fn promise_reject_callback(msg: v8::PromiseRejectMessage) {
    if msg.get_event() != v8::PromiseRejectEvent::PromiseRejectWithNoHandler {
        return;
    }
    // The callback runs inside V8's own scope; opening a second HandleScope
    // on the isolate would panic ("active scope can't be dropped") and abort.
    let scope = &mut unsafe { v8::CallbackScope::new(&msg) };
    if world_mut(scope).unhandled_rejection.is_some() {
        return;
    }
    // ponytail: first rejection wins; a later handler does not un-fail the
    // run. The value is stringified later, inside the guest context scope.
    match msg.get_value() {
        Some(v) => {
            let g = v8::Global::new(scope, v);
            world_mut(scope).unhandled_rejection_exc = Some(g);
        }
        None => world_mut(scope).unhandled_rejection = Some("unknown".to_string()),
    }
}

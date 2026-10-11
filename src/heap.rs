//! Heap-cap enforcement. The user cap is enforced below V8's own hard
//! limit: V8's heap may grow into a headroom above the cap, and a GC
//! prologue callback stops the guest once used memory passes the cap. A
//! near-heap-limit callback remains as a second line for paths that reach
//! V8's limit without a GC first. Both layers mark a termination reason
//! shared with the run's World before terminating.

use std::ffi::c_void;
use std::sync::Arc;

use crate::world::{TermReason, Termination};

/// What the heap callbacks need at trigger time. V8's near-heap-limit
/// callback receives no isolate reference, only the `data` pointer, so its
/// termination goes through the thread-safe handle; the GC prologue
/// callback receives the isolate and reads heap statistics through it.
pub(crate) struct HeapGuard {
    handle: v8::IsolateHandle,
    term: Arc<Termination>,
    cap_bytes: usize,
}

impl HeapGuard {
    pub(crate) fn new(handle: v8::IsolateHandle, term: Arc<Termination>, cap_bytes: usize) -> Self {
        HeapGuard {
            handle,
            term,
            cap_bytes,
        }
    }

    /// The `data` argument for the V8 callbacks. The guard must outlive the
    /// isolate: teardown runs GC, which calls back in through this pointer.
    pub(crate) fn callback_data(&mut self) -> *mut c_void {
        self as *mut Self as *mut c_void
    }
}

/// First line: before every GC, stop the guest whose heap has passed the
/// user cap. V8 still has the headroom left to collect and unwind, so its
/// fatal out-of-memory path (which aborts the whole process) is not
/// reached - several allocation shapes (tables, object keys) skip the
/// near-heap-limit callback entirely. V8's prologue is the crate's
/// equivalent of an epilogue callback; used heap is read before the
/// collection, i.e. at the pressure peak. A last-resort GC
/// (COLLECT_ALL_AVAILABLE_GARBAGE) also stops the guest: guests cannot
/// force or low-memory-trigger GCs here, so that flag only appears on
/// V8's fatal allocation-retry path, which can sit below the cap (for
/// example when a doubling table needs more than the cautious growth
/// limit allows). Inert once the run has concluded: teardown GCs may
/// legitimately still be over the cap.
pub(crate) extern "C" fn before_gc(
    isolate: *mut v8::Isolate,
    _ty: v8::GCType,
    flags: v8::GCCallbackFlags,
    data: *mut c_void,
) {
    let guard = unsafe { &*(data as *const HeapGuard) };
    if guard.term.is_finished() || guard.term.reason().is_some() {
        return;
    }
    // OR is idempotent, so `flags | X == flags` tests whether X's bits are
    // set without needing a BitAnd on the crate's flag type.
    let last_resort =
        (flags | v8::GCCallbackFlags::COLLECT_ALL_AVAILABLE_GARBAGE) == flags;
    let iso = unsafe { &mut *isolate };
    let mut stats = v8::HeapStatistics::default();
    iso.get_heap_statistics(&mut stats);
    if last_resort || stats.used_heap_size() > guard.cap_bytes {
        guard.term.mark(TermReason::Oom);
        guard.handle.terminate_execution();
    }
}

/// Second line: V8 is close to its own (raised) limit without the GC check
/// having stopped the guest first. Mark OOM, terminate, and hand V8 enough
/// extra room to unwind instead of taking its FatalProcessOutOfMemory
/// path, which would abort the whole process. A second invocation while
/// unwinding only re-marks and re-terminates, which changes nothing.
pub(crate) extern "C" fn on_near_heap_limit(
    data: *mut c_void,
    current: usize,
    _initial: usize,
) -> usize {
    let guard = unsafe { &*(data as *const HeapGuard) };
    guard.term.mark(TermReason::Oom);
    guard.handle.terminate_execution();
    current + (current / 4).max(16 << 20)
}

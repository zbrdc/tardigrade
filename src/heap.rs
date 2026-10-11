//! Heap-cap enforcement: V8's near-heap-limit callback turns into a clean
//! guest termination instead of a process-wide fatal out-of-memory abort.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the callback needs at trigger time. V8 passes it only the `data`
/// pointer (no isolate reference), so termination goes through the
/// thread-safe handle and the flag is shared with the run's World.
pub(crate) struct HeapGuard {
    handle: v8::IsolateHandle,
    oom: Arc<AtomicBool>,
}

impl HeapGuard {
    pub(crate) fn new(handle: v8::IsolateHandle, oom: Arc<AtomicBool>) -> Self {
        HeapGuard { handle, oom }
    }

    /// The `data` argument for add_near_heap_limit_callback. The guard must
    /// outlive the isolate: teardown can run GC, which may near the limit.
    pub(crate) fn callback_data(&mut self) -> *mut c_void {
        self as *mut Self as *mut c_void
    }
}

/// Terminates the guest when the heap nears the cap and raises the limit a
/// notch so V8 can unwind instead of taking its FatalProcessOutOfMemory
/// path, which would abort the whole process. A second invocation while
/// unwinding only re-marks and re-terminates, which changes nothing.
pub(crate) extern "C" fn on_near_heap_limit(
    data: *mut c_void,
    current: usize,
    _initial: usize,
) -> usize {
    let guard = unsafe { &*(data as *const HeapGuard) };
    guard.oom.store(true, Ordering::SeqCst);
    guard.handle.terminate_execution();
    current + (current >> 2)
}

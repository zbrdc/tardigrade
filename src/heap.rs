//! Heap-cap enforcement. The user cap (JS heap plus ArrayBuffer/external
//! memory together) is judged where the figure is meaningful.
//!
//! A maximal collection (`COLLECT_ALL_AVAILABLE_GARBAGE`, the
//! external-memory pressure path) is judged and terminated right there.
//! At host checkpoints (event-loop turns) the raw figure still contains
//! garbage accumulated since the last collection, so a maximal collection
//! is forced first and only the surviving live set is judged. The
//! near-heap-limit callback fires only after collections failed to keep
//! the heap under V8's own (raised) limit, meaning the guest's transient
//! working set (live data plus the allocation being satisfied) has
//! reached that limit, three times the cap: the run is ended there, and
//! the limit is still raised so the pending allocation succeeds and
//! JavaScript resumes into the termination instead of dying inside V8's
//! allocation retry, which never returns to JavaScript and ignores
//! termination. Finally, a watch thread polls the ArrayBuffer byte count,
//! because external growth alone never triggers a V8 collection in this
//! build: it asks for a collecting interrupt when the count passes the
//! cap and terminates after sustained over-cap polls.
//!
//! Judging the raw pre-GC figure at an ordinary collection (an earlier
//! design) terminates garbage-heavy guests whose live set never approaches
//! the cap; an interrupt request queued from inside a GC callback
//! (`IsolateHandle::request_interrupt`, also tried) is never delivered for
//! tight allocation loops. Neither is used.
//!
//! Judging the raw pre-GC figure at an ordinary collection (an earlier
//! design) terminates garbage-heavy guests whose live set never approaches
//! the cap; an interrupt request queued from inside a GC callback
//! (`IsolateHandle::request_interrupt`, also tried) is never delivered for
//! tight allocation loops. Neither is used.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::world::{world_mut, TermReason, Termination};

/// Smallest workable cap: V8 cannot sustain less (its own check failures
/// and fatal OOMs dominate below this).
pub(crate) const MIN_CAP_MB: usize = 8;

/// The cap plan for a run: the effective cap in MB and V8's raised limit
/// in bytes. The headroom (twice the cap, at least 32 MiB) lets ordinary
/// collections keep garbage-heavy guests running: their transient working
/// set stays under V8's limit, and only a live set (or sustained churn)
/// that genuinely outgrows the cap is stopped. Saturating arithmetic and a
/// pre-shift ceiling, because a library caller can pass any usize (a plain
/// `*`/`<<` would panic a debug build).
pub(crate) fn plan(heap_mb: usize) -> (usize, usize) {
    let cap_mb = heap_mb.max(MIN_CAP_MB);
    let headroom_mb = cap_mb.saturating_mul(2).max(32);
    let v8_max_mb = cap_mb.saturating_add(headroom_mb).min(usize::MAX >> 20);
    let v8_max_bytes = if v8_max_mb == usize::MAX >> 20 {
        usize::MAX
    } else {
        v8_max_mb << 20
    };
    (cap_mb, v8_max_bytes)
}

/// Used heap plus external (ArrayBuffer backing) memory, the figure the
/// cap is judged on.
fn used_plus_external(iso: &mut v8::Isolate, external: &AtomicUsize) -> usize {
    let mut stats = v8::HeapStatistics::default();
    iso.get_heap_statistics(&mut stats);
    stats
        .used_heap_size()
        .saturating_add(stats.external_memory())
        .saturating_add(external.load(Ordering::Relaxed))
}

/// True when `flags` carries the maximal-collection flag. The crate's flag
/// type has no BitAnd, but OR is idempotent, so `flags | X == flags` tests
/// whether X's bits are set.
fn is_maximal(flags: v8::GCCallbackFlags) -> bool {
    (flags | v8::GCCallbackFlags::COLLECT_ALL_AVAILABLE_GARBAGE) == flags
}

/// The ArrayBuffer allocator for a run: the platform allocator wrapped
/// with a live byte count, because this V8 build does not account sandboxed
/// backing stores as external memory (HeapStatistics::external_memory
/// stays near zero while ArrayBuffers grow), so the count must come from
/// the allocator itself.
fn counting_vtable() -> &'static v8::RustAllocatorVtable<AtomicUsize> {
    &v8::RustAllocatorVtable {
        allocate,
        allocate_uninitialized,
        free,
        reallocate,
        drop: drop_handle,
    }
}

/// Backing stores need pointer alignment; 16 matches what malloc gives.
const ALIGN: usize = 16;

fn layout(len: usize) -> std::alloc::Layout {
    // std alloc forbids zero-sized layouts; allocate one byte instead and
    // apply the same rule on free and realloc.
    std::alloc::Layout::from_size_align(len.max(1), ALIGN).unwrap()
}

/// These run inside V8: no panics (allocation failure returns null, which
/// V8 reports to the guest as a RangeError) and no calls back into V8.
unsafe extern "C" fn allocate(count: &AtomicUsize, len: usize) -> *mut c_void {
    count.fetch_add(len, Ordering::Relaxed);
    unsafe { std::alloc::alloc_zeroed(layout(len)) as *mut c_void }
}

unsafe extern "C" fn allocate_uninitialized(count: &AtomicUsize, len: usize) -> *mut c_void {
    count.fetch_add(len, Ordering::Relaxed);
    unsafe { std::alloc::alloc(layout(len)) as *mut c_void }
}

unsafe extern "C" fn free(count: &AtomicUsize, data: *mut c_void, len: usize) {
    count.fetch_sub(len, Ordering::Relaxed);
    unsafe { std::alloc::dealloc(data as *mut u8, layout(len)) };
}

unsafe extern "C" fn reallocate(
    count: &AtomicUsize,
    data: *mut c_void,
    old_len: usize,
    new_len: usize,
) -> *mut c_void {
    count.fetch_sub(old_len, Ordering::Relaxed);
    count.fetch_add(new_len, Ordering::Relaxed);
    unsafe {
        std::alloc::realloc(data as *mut u8, layout(old_len), layout(new_len).size()) as *mut c_void
    }
}

unsafe extern "C" fn drop_handle(count: *const AtomicUsize) {
    drop(unsafe { Arc::from_raw(count) });
}

/// The allocator feeding a shared live byte count (held by the run's
/// World and heap state).
pub(crate) fn counting_allocator(count: Arc<AtomicUsize>) -> v8::UniqueRef<v8::Allocator> {
    unsafe { v8::new_rust_allocator(Arc::into_raw(count), counting_vtable()) }
}

/// Watches the ArrayBuffer byte count from a side thread: external growth
/// alone never triggers a V8 collection in this build, so no GC callback
/// would fire for a guest that only allocates buffers. Every over-cap
/// poll requests an interrupt that collects and judges on the isolate
/// thread (dead wrappers only release their backing stores after a
/// collection); if the count stays over the cap for sustained polls -
/// interrupt delivery or not - the watch terminates the run itself.
pub(crate) struct HeapWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Over-cap polls (10 ms apart) tolerated before terminating: enough for
/// a collection requested through the interrupt to release dead wrappers.
const WATCH_STRIKES: usize = 10;

/// The guard pointer crossing into the watch thread. Sound because the
/// guard outlives the watch (both are dropped inside execute_inner's
/// scope, the watch first, and its thread is joined) and every field the
/// watch touches is atomic or immutable.
struct GuardPtr(*const HeapGuard);
unsafe impl Send for GuardPtr {}

impl HeapWatch {
    /// `guard` must outlive this watch (the watch is dropped inside
    /// execute_inner, the guard at the end of execute).
    pub(crate) fn new(handle: v8::IsolateHandle, guard: *const HeapGuard) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let guard = GuardPtr(guard);
        let thread = std::thread::Builder::new()
            .name("v8vm-heap-watch".to_string())
            .spawn(move || {
                let guard = guard;
                let guard = unsafe { &*guard.0 };
                let mut strikes = 0;
                while !stop2.load(Ordering::SeqCst)
                    && guard.term.reason().is_none()
                    && !guard.term.is_finished()
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    if guard.external.load(Ordering::Relaxed) > guard.cap_bytes {
                        strikes += 1;
                        let _ = handle.request_interrupt(reclaim, guard as *const HeapGuard as *mut c_void);
                        if strikes >= WATCH_STRIKES {
                            guard.term.mark(TermReason::Oom);
                            handle.terminate_execution();
                            return;
                        }
                    } else {
                        strikes = 0;
                    }
                }
            })
            .expect("spawn heap watch");
        HeapWatch {
            stop,
            thread: Some(thread),
        }
    }
}

/// Runs on the isolate thread from the interrupt the watch requested:
/// collect first (so buffers whose wrappers died are released), then
/// judge the surviving figure.
extern "C" fn reclaim(isolate: &mut v8::Isolate, data: *mut c_void) {
    let guard = unsafe { &*(data as *const HeapGuard) };
    if guard.term.is_finished() || guard.term.reason().is_some() {
        return;
    }
    isolate.low_memory_notification();
    if used_plus_external(isolate, &guard.external) > guard.cap_bytes {
        guard.term.mark(TermReason::Oom);
        guard.handle.terminate_execution();
    }
}

impl Drop for HeapWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The per-run heap-side state shared with V8's callbacks. The callbacks
/// receive only the `data` pointer plus (for GC callbacks) the isolate, so
/// termination goes through the thread-safe handle and the shared
/// termination reason.
pub(crate) struct HeapGuard {
    handle: v8::IsolateHandle,
    term: Arc<Termination>,
    cap_bytes: usize,
    /// Live ArrayBuffer bytes from the counting allocator (this build does
    /// not account them as external memory by itself).
    external: Arc<AtomicUsize>,
    /// Where the near-heap-limit rescue raises V8's limit to: the starting
    /// limit plus twice itself, room for one more growth step of a
    /// doubling shape so the termination is observed in JavaScript.
    limit_ceiling: usize,
}

impl HeapGuard {
    pub(crate) fn new(
        handle: v8::IsolateHandle,
        term: Arc<Termination>,
        cap_bytes: usize,
        external: Arc<AtomicUsize>,
        v8_max_bytes: usize,
    ) -> Self {
        HeapGuard {
            handle,
            term,
            cap_bytes,
            external,
            limit_ceiling: v8_max_bytes.saturating_mul(3),
        }
    }

    /// The `data` argument for the V8 callbacks. The guard must outlive the
    /// isolate: teardown runs GC, which calls back in through this pointer.
    pub(crate) fn callback_data(&mut self) -> *mut c_void {
        self as *mut Self as *mut c_void
    }
}

/// Registered for every GC type; only maximal collections (external-memory
/// pressure) are judged here, because their figure is what forced them.
pub(crate) extern "C" fn before_gc(
    isolate: *mut v8::Isolate,
    _ty: v8::GCType,
    flags: v8::GCCallbackFlags,
    data: *mut c_void,
) {
    if !is_maximal(flags) {
        return;
    }
    let guard = unsafe { &*(data as *const HeapGuard) };
    if guard.term.is_finished() || guard.term.reason().is_some() {
        return;
    }
    let iso = unsafe { &mut *isolate };
    if used_plus_external(iso, &guard.external) > guard.cap_bytes {
        guard.term.mark(TermReason::Oom);
        guard.handle.terminate_execution();
    }
}

/// Host checkpoint for event-loop turns, covering external-memory growth
/// that has not triggered a collection (ArrayBuffers alone need not make
/// V8 collect). The raw figure still contains garbage accumulated since
/// the last collection, so a maximal collection is forced first and only
/// the surviving live set is judged.
pub(crate) fn host_checkpoint(scope: &mut v8::HandleScope) {
    if world_mut(scope).term.reason().is_some() || world_mut(scope).term.is_finished() {
        return;
    }
    let cap_bytes = world_mut(scope).heap_cap_bytes;
    let external = world_mut(scope).external_bytes.clone();
    if used_plus_external(scope, &external) > cap_bytes {
        scope.low_memory_notification();
        if used_plus_external(scope, &external) > cap_bytes {
            world_mut(scope).term.mark(TermReason::Oom);
            scope.terminate_execution();
        }
    }
}

/// V8 invokes this after collections failed to keep the heap under its
/// limit, with the limit about to block an allocation. The guest's
/// transient working set has reached V8's raised limit (three times the
/// cap): end the run, and raise the limit to the ceiling so the pending
/// allocation succeeds and the termination is observed when JavaScript
/// resumes (growth by doubling otherwise dies inside the retry).
pub(crate) extern "C" fn on_near_heap_limit(
    data: *mut c_void,
    _current: usize,
    _initial: usize,
) -> usize {
    let guard = unsafe { &*(data as *const HeapGuard) };
    guard.term.mark(TermReason::Oom);
    guard.handle.terminate_execution();
    guard.limit_ceiling
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_clamps_the_minimum_and_adds_headroom() {
        assert_eq!(plan(0), (8, 40 << 20));
        assert_eq!(plan(7), (8, 40 << 20));
        assert_eq!(plan(64), (64, 192 << 20));
        assert_eq!(plan(256), (256, 768 << 20));
    }

    #[test]
    fn plan_saturates_instead_of_overflowing() {
        // A plain `3 * heap_mb << 20` would panic a debug build here.
        let (cap, v8_max) = plan(usize::MAX);
        assert_eq!(cap, usize::MAX);
        assert_eq!(v8_max, usize::MAX);
    }

    #[test]
    fn maximal_flag_is_detected_among_other_bits() {
        assert!(!is_maximal(v8::GCCallbackFlags::NONE));
        assert!(is_maximal(v8::GCCallbackFlags::COLLECT_ALL_AVAILABLE_GARBAGE));
        assert!(is_maximal(
            v8::GCCallbackFlags::COLLECT_ALL_AVAILABLE_GARBAGE
                | v8::GCCallbackFlags::COLLECT_ALL_EXTERNAL_MEMORY
        ));
        assert!(!is_maximal(v8::GCCallbackFlags::FORCED));
    }
}

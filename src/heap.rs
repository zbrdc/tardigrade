//! Heap-cap enforcement, in three cooperating pieces.
//!
//! A garbage-collection epilogue judges the JS heap: after a major
//! collection the used-heap figure is the live set, so a guest is stopped
//! (exit 137) exactly when its live data passes the cap, while a
//! garbage-heavy guest whose collections succeed keeps running. The
//! epilogue is registered through the engine's own (mangled) symbol: the
//! v8 crate binds only the prologue variant.
//!
//! The near-heap-limit callback is the second line: V8 invokes it only
//! after collections failed to keep the heap under its own (raised)
//! limit, and it still raises the limit so the pending allocation
//! succeeds and JavaScript resumes into the termination instead of dying
//! inside V8's allocation retry, which never returns to JavaScript and
//! ignores termination.
//!
//! ArrayBuffer memory shares the same single budget from the allocator
//! side: this V8 build does not account sandboxed backing stores as
//! external memory, so a counting allocator refuses any allocation that
//! would push live backing-store bytes plus the latest post-GC heap
//! figure past the cap; V8 turns the refusal into a catchable RangeError
//! the guest may survive. Judging the raw pre-GC figure (an earlier
//! design) terminates garbage-heavy guests whose live set never
//! approaches the cap, and an interrupt request queued from inside a GC
//! callback (`IsolateHandle::request_interrupt`, also tried) is never
//! delivered for tight allocation loops; neither is used.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::world::{TermReason, Termination};

extern "C" {
    /// v8::Isolate::AddGCEpilogueCallback(callback, data, gc_type_filter)
    /// by its mangled name: the crate binds only the prologue variant, and
    /// the engine exports this symbol from the same static library. The
    /// C++ member function takes `this` as its first argument, which is
    /// exactly this declaration.
    fn _ZN2v87Isolate21AddGCEpilogueCallbackEPFvPS0_NS_6GCTypeENS_15GCCallbackFlagsEPvES4_S2_(
        isolate: *mut v8::Isolate,
        callback: extern "C" fn(
            isolate: *mut v8::Isolate,
            ty: v8::GCType,
            flags: v8::GCCallbackFlags,
            data: *mut c_void,
        ),
        data: *mut c_void,
        gc_type_filter: v8::GCType,
    );
}

/// Smallest workable cap: V8 cannot sustain less (its own check failures
/// and fatal OOMs dominate below this).
pub(crate) const MIN_CAP_MB: usize = 8;

/// The cap plan for a run: the effective cap in MB, the cap in bytes, and
/// V8's raised limit in bytes. The headroom (twice the cap, at least
/// 32 MiB) keeps the fatal allocation-retry path away from guests that
/// are merely close to the cap, while the near-heap-limit rescue covers
/// genuinely over-cap growth. Saturating arithmetic and pre-shift
/// ceilings, because a library caller can pass any usize (a plain `*`/`<<`
/// would panic a debug build or wrap in release).
pub(crate) fn plan(heap_mb: usize) -> CapPlan {
    let cap_mb = heap_mb.max(MIN_CAP_MB);
    let headroom_mb = cap_mb.saturating_mul(2).max(32);
    let v8_max_mb = cap_mb.saturating_add(headroom_mb);
    CapPlan {
        cap_mb,
        cap_bytes: cap_mb.min(usize::MAX >> 20).saturating_mul(1 << 20),
        v8_max_bytes: v8_max_mb.min(usize::MAX >> 20).saturating_mul(1 << 20),
    }
}

/// The derived limits for one run (see [plan]).
pub(crate) struct CapPlan {
    pub(crate) cap_mb: usize,
    pub(crate) cap_bytes: usize,
    pub(crate) v8_max_bytes: usize,
}

/// The per-run heap state shared with V8's callbacks. The callbacks
/// receive only the `data` pointer plus (for GC callbacks) the isolate,
/// so termination goes through the thread-safe handle and the shared
/// termination reason.
pub(crate) struct HeapGuard {
    handle: v8::IsolateHandle,
    term: Arc<Termination>,
    cap_bytes: usize,
    /// The latest post-GC used-heap figure, shared with the counting
    /// allocator so heap and buffers share one budget.
    heap_used: Arc<AtomicUsize>,
    /// Where the near-heap-limit rescue raises V8's limit to: the starting
    /// limit plus twice itself, room for one more growth step of a
    /// doubling shape so the termination is observed in JavaScript.
    limit_ceiling: usize,
}

impl HeapGuard {
    pub(crate) fn new(
        handle: v8::IsolateHandle,
        term: Arc<Termination>,
        plan: &CapPlan,
        heap_used: Arc<AtomicUsize>,
    ) -> Self {
        HeapGuard {
            handle,
            term,
            cap_bytes: plan.cap_bytes,
            heap_used,
            limit_ceiling: plan.v8_max_bytes.saturating_mul(3),
        }
    }

    /// The `data` argument for the V8 callbacks. The guard must outlive the
    /// isolate: teardown runs GC, which calls back in through this pointer.
    pub(crate) fn callback_data(&mut self) -> *mut c_void {
        self as *mut Self as *mut c_void
    }

    /// Registers the heap-limit callback and the GC epilogue on `isolate`;
    /// kept next to the guard so the callback data and the registrations
    /// cannot drift apart.
    pub(crate) fn register(&mut self, isolate: &mut v8::Isolate) {
        let data = self.callback_data();
        isolate.add_near_heap_limit_callback(on_near_heap_limit, data);
        unsafe {
            _ZN2v87Isolate21AddGCEpilogueCallbackEPFvPS0_NS_6GCTypeENS_15GCCallbackFlagsEPvES4_S2_(
                isolate,
                after_gc,
                data,
                v8::GCType::ALL,
            )
        };
    }
}

/// After a collection: publish the heap figure for the shared budget and,
/// after a major collection, stop a guest whose live set passed the cap.
/// A borderline figure simply waits for the next collection, which a
/// garbage-heavy guest passes and a growing one does not.
pub(crate) extern "C" fn after_gc(
    isolate: *mut v8::Isolate,
    ty: v8::GCType,
    _flags: v8::GCCallbackFlags,
    data: *mut c_void,
) {
    let guard = unsafe { &*(data as *const HeapGuard) };
    let iso = unsafe { &mut *isolate };
    let mut stats = v8::HeapStatistics::default();
    iso.get_heap_statistics(&mut stats);
    let used = stats.used_heap_size();
    guard.heap_used.store(used, Ordering::Relaxed);
    if ty != v8::GCType::MARK_SWEEP_COMPACT {
        return;
    }
    if guard.term.is_finished() || guard.term.reason().is_some() {
        return;
    }
    if used > guard.cap_bytes {
        guard.term.mark(TermReason::Oom);
        guard.handle.terminate_execution();
    }
}

fn counting_vtable() -> &'static v8::RustAllocatorVtable<AllocState> {
    &v8::RustAllocatorVtable {
        allocate,
        allocate_uninitialized,
        free,
        reallocate,
        drop: drop_handle,
    }
}

/// The allocator's view of the shared budget: its own live byte count
/// against the cap minus the latest heap figure.
pub(crate) struct AllocState {
    live: AtomicUsize,
    cap_bytes: usize,
    heap_used: Arc<AtomicUsize>,
}

impl AllocState {
    /// True when `extra` more backing-store bytes fit under the budget.
    fn fits(&self, extra: usize) -> bool {
        let budget = self
            .cap_bytes
            .saturating_sub(self.heap_used.load(Ordering::Relaxed));
        self.live
            .load(Ordering::Relaxed)
            .checked_add(extra)
            .map(|need| need <= budget)
            .unwrap_or(false)
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
/// V8 reports to the guest as a catchable RangeError) and no calls back
/// into V8. The count only moves after a successful allocation.
unsafe extern "C" fn allocate(st: &AllocState, len: usize) -> *mut c_void {
    if !st.fits(len) {
        return std::ptr::null_mut();
    }
    let ptr = unsafe { std::alloc::alloc_zeroed(layout(len)) };
    if !ptr.is_null() {
        st.live.fetch_add(len, Ordering::Relaxed);
    }
    ptr as *mut c_void
}

unsafe extern "C" fn allocate_uninitialized(st: &AllocState, len: usize) -> *mut c_void {
    if !st.fits(len) {
        return std::ptr::null_mut();
    }
    let ptr = unsafe { std::alloc::alloc(layout(len)) };
    if !ptr.is_null() {
        st.live.fetch_add(len, Ordering::Relaxed);
    }
    ptr as *mut c_void
}

unsafe extern "C" fn free(st: &AllocState, data: *mut c_void, len: usize) {
    if !data.is_null() {
        st.live.fetch_sub(len, Ordering::Relaxed);
        unsafe { std::alloc::dealloc(data as *mut u8, layout(len)) };
    }
}

unsafe extern "C" fn reallocate(
    st: &AllocState,
    data: *mut c_void,
    old_len: usize,
    new_len: usize,
) -> *mut c_void {
    if data.is_null() || old_len == 0 {
        return unsafe { allocate_uninitialized(st, new_len) };
    }
    if !st.fits(new_len.saturating_sub(old_len)) {
        return std::ptr::null_mut();
    }
    let ptr = unsafe {
        std::alloc::realloc(data as *mut u8, layout(old_len), layout(new_len).size())
    };
    if !ptr.is_null() {
        st.live.fetch_sub(old_len, Ordering::Relaxed);
        st.live.fetch_add(new_len, Ordering::Relaxed);
    }
    ptr as *mut c_void
}

unsafe extern "C" fn drop_handle(st: *const AllocState) {
    drop(unsafe { Arc::from_raw(st) });
}

/// The counting allocator for a run: enforces the ArrayBuffer half of the
/// shared budget by refusing allocations that do not fit.
pub(crate) fn counting_allocator(
    cap_bytes: usize,
    heap_used: Arc<AtomicUsize>,
) -> v8::UniqueRef<v8::Allocator> {
    let state = Arc::new(AllocState {
        live: AtomicUsize::new(0),
        cap_bytes,
        heap_used,
    });
    unsafe { v8::new_rust_allocator(Arc::into_raw(state), counting_vtable()) }
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
        let p = plan(0);
        assert_eq!((p.cap_mb, p.cap_bytes), (8, 8 << 20));
        assert_eq!(p.v8_max_bytes, 40 << 20);
        assert_eq!(plan(7).cap_mb, 8);
        assert_eq!(plan(64).v8_max_bytes, 192 << 20);
        assert_eq!(plan(256).v8_max_bytes, 768 << 20);
    }

    #[test]
    fn plan_saturates_instead_of_overflowing() {
        // A plain `3 * heap_mb << 20` would panic a debug build here and
        // wrap in release (a 16 TiB cap became 0 bytes before).
        let p = plan(usize::MAX);
        assert_eq!(p.cap_mb, usize::MAX);
        assert!(p.cap_bytes >= usize::MAX >> 21, "no wrap to a small value");
        assert!(p.v8_max_bytes >= p.cap_bytes);
        let p = plan(1 << 44); // 16 TiB, a value a user can actually pass
        assert!(p.cap_bytes >= 1 << 44, "a 16 TiB cap must not wrap to 0");
        assert!(p.v8_max_bytes >= p.cap_bytes);
    }

    #[test]
    fn allocator_state_enforces_the_shared_budget() {
        let heap_used = Arc::new(AtomicUsize::new(0));
        let st = AllocState {
            live: AtomicUsize::new(10 << 20),
            cap_bytes: 32 << 20,
            heap_used: heap_used.clone(),
        };
        assert!(st.fits(22 << 20));
        assert!(!st.fits(23 << 20), "over the cap");
        heap_used.store(20 << 20, Ordering::Relaxed);
        assert!(!st.fits(13 << 20), "the heap figure eats the budget");
        assert!(st.fits(2 << 20));
        assert!(!st.fits(3 << 20));
        assert!(!st.fits(usize::MAX), "overflow does not fit");
    }
}

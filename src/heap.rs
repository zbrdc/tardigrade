//! Heap-cap enforcement, in two independent pieces with supported APIs
//! only.
//!
//! The JS heap is capped by V8's own near-heap-limit callback, registered
//! through the crate's `add_near_heap_limit_callback`. V8 invokes it only
//! after its collections failed to keep the heap under the configured
//! limit, so the callback is post-GC by construction: a guest whose
//! collections succeed (churn with a small live set) never trips it, and
//! genuinely growing guests do. The callback marks the OOM termination,
//! requests the termination, and returns a raised limit with generous
//! unwinding headroom so the allocation that triggered it succeeds and
//! JavaScript resumes straight into the termination - growth otherwise
//! dies inside V8's allocation retry, which never returns to JavaScript
//! and ignores termination. Where a run is actually stopped therefore sits
//! between the configured cap and the raised limit (README states this).
//!
//! ArrayBuffer memory is capped separately by the counting allocator:
//! this V8 build does not account sandboxed backing stores as external
//! memory, so the allocator enforces its own budget (live backing-store
//! bytes against the same --memory figure) by refusing allocations that
//! would cross it; V8 reports the refusal to the guest as a catchable
//! RangeError it may survive. Resizable ArrayBuffers and growable
//! SharedArrayBuffers bypass the allocator entirely (see README).

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::world::{TermReason, Termination};

/// Smallest workable cap: V8 cannot sustain less (its own check failures
/// and fatal OOMs dominate below this).
pub(crate) const MIN_CAP_MB: usize = 8;

/// The cap plan for a run: the effective cap in MB, the cap in bytes, and
/// V8's own heap limit in bytes. Saturating arithmetic and pre-shift
/// ceilings, because a library caller can pass any usize (a plain `*`/`<<`
/// would panic a debug build or wrap in release).
pub(crate) fn plan(heap_mb: usize) -> CapPlan {
    let cap_mb = heap_mb.max(MIN_CAP_MB);
    let v8_max_mb = cap_mb.saturating_add(headroom_mb(cap_mb));
    CapPlan {
        cap_mb,
        cap_bytes: cap_mb.min(usize::MAX >> 20).saturating_mul(1 << 20),
        v8_max_bytes: v8_max_mb.min(usize::MAX >> 20).saturating_mul(1 << 20),
    }
}

/// Headroom V8 gets above the cap (probing max = cap exactly first).
fn headroom_mb(cap_mb: usize) -> usize {
    cap_mb.saturating_mul(2).max(32)
}

/// The derived limits for one run (see [plan]).
pub(crate) struct CapPlan {
    pub(crate) cap_mb: usize,
    pub(crate) cap_bytes: usize,
    pub(crate) v8_max_bytes: usize,
}

/// The per-run state V8's near-heap-limit callback needs. The callback
/// receives only the `data` pointer, so termination goes through the
/// thread-safe handle and the shared termination reason.
pub(crate) struct HeapGuard {
    handle: v8::IsolateHandle,
    term: Arc<Termination>,
    /// Where the callback raises V8's limit to, so the pending allocation
    /// succeeds and the termination is observed in JavaScript.
    limit_ceiling: usize,
}

impl HeapGuard {
    pub(crate) fn new(handle: v8::IsolateHandle, term: Arc<Termination>, plan: &CapPlan) -> Self {
        HeapGuard {
            handle,
            term,
            limit_ceiling: plan.v8_max_bytes.saturating_mul(3),
        }
    }

    /// Registers the heap-limit callback; kept next to the guard so the
    /// callback data and the registration cannot drift apart.
    pub(crate) fn register(&mut self, isolate: &mut v8::Isolate) {
        let data = self as *mut Self as *mut c_void;
        isolate.add_near_heap_limit_callback(on_near_heap_limit, data);
    }
}

/// V8 invokes this after collections failed to keep the heap under its
/// limit, with the limit about to block an allocation. Mark the OOM
/// termination, request it, and raise the limit generously so the pending
/// allocation succeeds and JavaScript resumes into the termination
/// instead of dying inside V8's retry. After the run has concluded only
/// the raise happens (see the finished check below).
pub(crate) extern "C" fn on_near_heap_limit(
    data: *mut c_void,
    current: usize,
    _initial: usize,
) -> usize {
    let guard = unsafe { &*(data as *const HeapGuard) };
    // Once the run has concluded (teardown GCs, end-of-run work that still
    // runs V8) a mark would be consumed by nobody and a termination would
    // be swallowed by that work: raise the limit and stay out of the way.
    if !guard.term.is_finished() {
        guard.term.mark(TermReason::Oom);
        guard.handle.terminate_execution();
    }
    guard.limit_ceiling.max(current)
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

/// The allocator's budget: live backing-store bytes against the cap. It
/// is independent of the JS-heap figure (--memory caps each side at N).
pub(crate) struct AllocState {
    live: AtomicUsize,
    cap_bytes: usize,
}

impl AllocState {
    /// True when `extra` more backing-store bytes fit under the budget.
    fn fits(&self, extra: usize) -> bool {
        self.live
            .load(Ordering::Relaxed)
            .checked_add(extra)
            .map(|need| need <= self.cap_bytes)
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
    if data.is_null() {
        return unsafe { allocate_uninitialized(st, new_len) };
    }
    if old_len == 0 {
        // A zero-length block is our one-byte allocation; grow into a
        // fresh block and release the old one.
        let ptr = unsafe { allocate_uninitialized(st, new_len) };
        if !ptr.is_null() {
            unsafe { std::alloc::dealloc(data as *mut u8, layout(0)) };
        }
        return ptr;
    }
    if new_len > old_len && !st.fits(new_len - old_len) {
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

/// The counting allocator for a run: enforces the ArrayBuffer side of
/// --memory by refusing allocations that do not fit.
pub(crate) fn counting_allocator(cap_bytes: usize) -> v8::UniqueRef<v8::Allocator> {
    let state = Arc::new(AllocState {
        live: AtomicUsize::new(0),
        cap_bytes,
    });
    unsafe { v8::new_rust_allocator(Arc::into_raw(state), counting_vtable()) }
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
    fn allocator_state_enforces_its_budget() {
        let st = AllocState {
            live: AtomicUsize::new(10 << 20),
            cap_bytes: 32 << 20,
        };
        assert!(st.fits(22 << 20));
        assert!(!st.fits(23 << 20), "over the cap");
        assert!(!st.fits(usize::MAX), "overflow does not fit");
    }
}

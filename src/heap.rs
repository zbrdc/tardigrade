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
//! RangeError it may survive. Buffers the host builds from Rust bytes
//! (TextEncoder.encode, vm.fs.readBytes) are charged to the same budget
//! through [uint8_array_from], so they cannot be retained past it either.
//! Resizable ArrayBuffers and growable SharedArrayBuffers bypass the
//! allocator entirely (see README).

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::world::{world_mut, TermReason, Termination};

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

/// What the near-heap-limit callback decides: the limit to return and
/// whether the isolate must be terminated. Once the run has concluded
/// (teardown GCs, any end-of-run work that still runs V8) a mark would be
/// consumed by nobody and a termination would be swallowed by that work,
/// so only the raise happens.
fn limit_decision(term: &Termination, current: usize, ceiling: usize) -> (usize, bool) {
    let terminate = !term.is_finished();
    if terminate {
        term.mark(TermReason::Oom);
    }
    (ceiling.max(current), terminate)
}

/// V8 invokes this after collections failed to keep the heap under its
/// limit, with the limit about to block an allocation. Mark the OOM
/// termination, request it, and raise the limit generously so the pending
/// allocation succeeds and JavaScript resumes into the termination
/// instead of dying inside V8's retry (see [limit_decision]).
pub(crate) extern "C" fn on_near_heap_limit(
    data: *mut c_void,
    current: usize,
    _initial: usize,
) -> usize {
    let guard = unsafe { &*(data as *const HeapGuard) };
    let (limit, terminate) = limit_decision(&guard.term, current, guard.limit_ceiling);
    if terminate {
        guard.handle.terminate_execution();
    }
    limit
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
    /// A budget of `cap_bytes` with nothing allocated yet.
    pub(crate) fn new(cap_bytes: usize) -> Self {
        AllocState {
            live: AtomicUsize::new(0),
            cap_bytes,
        }
    }

    /// True when `extra` more backing-store bytes fit under the budget.
    fn fits(&self, extra: usize) -> bool {
        self.live
            .load(Ordering::Relaxed)
            .checked_add(extra)
            .map(|need| need <= self.cap_bytes)
            .unwrap_or(false)
    }

    /// Reserves `len` bytes atomically; false (nothing reserved) when they
    /// do not fit.
    fn try_charge(&self, len: usize) -> bool {
        self.live
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
                live.checked_add(len).filter(|&need| need <= self.cap_bytes)
            })
            .is_ok()
    }
}

/// Rust-owned bytes whose length is charged to the ArrayBuffer budget for
/// as long as V8 keeps them as a backing store.
struct Charged {
    bytes: Vec<u8>,
    budget: Arc<AllocState>,
}

impl AsMut<[u8]> for Charged {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl Drop for Charged {
    fn drop(&mut self) {
        self.budget.live.fetch_sub(self.bytes.len(), Ordering::SeqCst);
    }
}

/// A Uint8Array over `bytes` that the run's ArrayBuffer budget has
/// accepted; None when the budget refuses (callers raise their own
/// RangeError) or V8 cannot build the view. The charge ends when V8 frees
/// the backing store.
pub(crate) fn uint8_array_from<'s>(
    scope: &mut v8::HandleScope<'s>,
    bytes: Vec<u8>,
) -> Option<v8::Local<'s, v8::Uint8Array>> {
    let budget = world_mut(scope).ab_budget.clone();
    // V8 collects garbage and retries when its allocator refuses; the same
    // courtesy here, or a guest that churns through unreferenced buffers
    // would see refusals for memory only the collector can return.
    if !budget.try_charge(bytes.len()) {
        scope.low_memory_notification();
        if !budget.try_charge(bytes.len()) {
            return None;
        }
    }
    let charged = Box::new(Charged { bytes, budget });
    let store = v8::ArrayBuffer::new_backing_store_from_bytes(charged).make_shared();
    let len = store.byte_length();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    v8::Uint8Array::new(scope, buffer, 0, len)
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
/// --memory by refusing allocations that do not fit the shared budget.
pub(crate) fn counting_allocator(budget: &Arc<AllocState>) -> v8::UniqueRef<v8::Allocator> {
    unsafe { v8::new_rust_allocator(Arc::into_raw(budget.clone()), counting_vtable()) }
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
    fn the_limit_callback_marks_and_terminates_while_the_run_is_live() {
        let term = Termination::new();
        let (limit, terminate) = limit_decision(&term, 10 << 20, 100 << 20);
        assert_eq!(limit, 100 << 20);
        assert!(terminate);
        assert!(term.reason() == Some(TermReason::Oom));
    }

    #[test]
    fn the_limit_callback_only_raises_once_the_run_has_finished() {
        let term = Termination::new();
        term.finish();
        let (limit, terminate) = limit_decision(&term, 10 << 20, 100 << 20);
        assert_eq!(limit, 100 << 20, "still raised, so the allocation succeeds");
        assert!(!terminate, "no termination after the run concluded");
        assert!(term.reason().is_none(), "no mark after the run concluded");
        // A current limit above the ceiling is never lowered.
        assert_eq!(limit_decision(&term, 200 << 20, 100 << 20).0, 200 << 20);
    }

    #[test]
    fn allocator_state_enforces_its_budget() {
        let st = AllocState::new(32 << 20);
        assert!(st.try_charge(10 << 20));
        assert!(st.fits(22 << 20));
        assert!(!st.fits(23 << 20), "over the cap");
        assert!(!st.fits(usize::MAX), "overflow does not fit");
    }

    #[test]
    fn charges_are_atomic_and_refused_past_the_budget() {
        let st = AllocState::new(100);
        assert!(st.try_charge(60));
        assert!(!st.try_charge(41), "60 + 41 is over 100");
        assert!(st.try_charge(40), "a refusal reserves nothing");
        assert!(!st.try_charge(1));
        assert!(!st.try_charge(usize::MAX), "overflow is refused");
    }

    #[test]
    fn dropping_charged_bytes_returns_them_to_the_budget() {
        let budget = Arc::new(AllocState::new(100));
        assert!(budget.try_charge(30));
        let held = Charged { bytes: vec![0; 30], budget: budget.clone() };
        assert!(!budget.try_charge(71));
        drop(held);
        assert!(budget.try_charge(100), "the 30 bytes came back");
    }
}

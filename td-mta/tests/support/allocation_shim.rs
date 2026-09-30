use super::allocation_counter::Counters;
use std::alloc::{GlobalAlloc, Layout, System};

pub static TD_MTA_ALLOCATION_COUNTERS: Counters = Counters::new();

#[global_allocator]
static ALLOCATOR: RustAllocationProbe = RustAllocationProbe;
struct RustAllocationProbe;

// SAFETY: forward each valid caller layout/pointer unchanged to System. The
// counter path uses only non-unwinding scalar/atomic operations, never storage.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for RustAllocationProbe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero layout.
        let result = unsafe { System.alloc(layout) };
        TD_MTA_ALLOCATION_COUNTERS.allocated(layout.size(), false, !result.is_null());
        result
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract; System preserves initialization and alignment.
        let result = unsafe { System.alloc_zeroed(layout) };
        TD_MTA_ALLOCATION_COUNTERS.allocated(layout.size(), true, !result.is_null());
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        TD_MTA_ALLOCATION_COUNTERS.freed(layout.size());
        // SAFETY: caller supplies a live pointer and its matching layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: caller supplies a live pointer, matching layout and valid size.
        let result = unsafe { System.realloc(ptr, layout, new_size) };
        TD_MTA_ALLOCATION_COUNTERS.resized(layout.size(), new_size, !result.is_null());
        result
    }
}

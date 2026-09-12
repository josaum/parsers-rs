use pdf_fast_core::{ParseOptions, PdfDocument};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const PROBE_LEN: usize = 4_093;

struct TrackingAllocator;

static TRACK_PROBE_ALLOCATION: AtomicBool = AtomicBool::new(false);
static PROBE_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn record_probe_allocation(bytes: usize) {
    if TRACK_PROBE_ALLOCATION.load(Ordering::Relaxed) && bytes == PROBE_LEN {
        PROBE_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_probe_allocation(layout.size());
        // SAFETY: forwarding the unchanged layout to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_probe_allocation(layout.size());
        // SAFETY: forwarding the unchanged layout to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` was returned by this allocator, which delegates to
        // the system allocator with the same layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_probe_allocation(new_size);
        // SAFETY: forwarding the original pointer/layout and requested size to
        // the same system allocator that created the allocation.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

#[test]
fn oversized_borrowed_input_is_rejected_before_source_sized_copy() {
    let bytes = vec![b'X'; PROBE_LEN];
    PROBE_ALLOCATIONS.store(0, Ordering::Relaxed);
    TRACK_PROBE_ALLOCATION.store(true, Ordering::Relaxed);
    let result = PdfDocument::from_slice(
        &bytes,
        ParseOptions {
            max_document_bytes: PROBE_LEN - 1,
            ..ParseOptions::default()
        },
    );
    TRACK_PROBE_ALLOCATION.store(false, Ordering::Relaxed);

    let error = result.expect_err("oversized borrowed PDF input must be rejected");
    assert!(
        error.to_string().contains("max_document_bytes 4092"),
        "{error:#}"
    );
    assert_eq!(
        PROBE_ALLOCATIONS.load(Ordering::Relaxed),
        0,
        "from_slice copied the full borrowed input before enforcing the limit"
    );
}

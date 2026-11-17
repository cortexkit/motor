//! Calling-thread heap high-water measurements, excluding fixture setup.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

struct Counting;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn change(bytes: i64) {
    let _ = ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = LIVE.try_with(|live| {
                live.set(live.get() + bytes);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get().max(0) as usize)));
            });
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            change(layout.size() as i64);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            change(layout.size() as i64);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        change(-(layout.size() as i64));
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = System.realloc(ptr, layout, size);
        if !result.is_null() {
            // Count both blocks at the resize high-water mark even if the system
            // allocator can grow the old block in place.
            change(size as i64);
            change(-(layout.size() as i64));
        }
        result
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Measure synchronous work that neither frees pre-existing allocations nor
/// transfers allocations to another thread. Fixtures must be built beforehand.
pub(crate) fn peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            ENABLED.with(|enabled| enabled.set(false));
        }
    }
    ENABLED.with(|enabled| assert!(!enabled.replace(true)));
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    let stop = Stop;
    let value = f();
    drop(stop);
    (value, PEAK.with(Cell::get))
}

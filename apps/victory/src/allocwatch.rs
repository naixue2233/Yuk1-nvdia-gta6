//! A global allocator that reports how much it holds, when `VICTORY_ALLOC_TRACE`
//! is set. The batch translator (`victory shaders`) held some 5.3 GB of working
//! set for a handful of small programs; this says whether the Rust heap holds
//! that, and when it starts holding it.
//!
//! Off unless the switch is there: two atomic adds per allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ON: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static HIGH: AtomicUsize = AtomicUsize::new(0);

const STEP: usize = 128 << 20;

/// Turned on from `main`, before the first shader is read.
pub fn arm() {
    let on = std::env::var_os("VICTORY_ALLOC_TRACE").is_some();
    ON.store(on, Ordering::Relaxed);
    if on {
        eprintln!("alloc trace on: the live heap is reported every 128 MiB it gains");
    }
}

fn grew(live: usize) {
    let high = HIGH.load(Ordering::Relaxed);
    if live < high + STEP {
        return;
    }
    if HIGH
        .compare_exchange(high, live, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        eprintln!(
            "ALLOC-LIVE {:.0} MiB",
            live as f64 / (1024.0 * 1024.0)
        );
    }
}

pub struct Watch;

unsafe impl GlobalAlloc for Watch {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if ON.load(Ordering::Relaxed) && size >= 4096 {
            let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
            grew(live);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if ON.load(Ordering::Relaxed) && size >= 4096 {
            let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
            grew(live);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ON.load(Ordering::Relaxed) {
            if new_size > layout.size() {
                let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                    + (new_size - layout.size());
                grew(live);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ON.load(Ordering::Relaxed) && layout.size() >= 4096 {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

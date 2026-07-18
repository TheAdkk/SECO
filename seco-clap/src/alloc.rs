//! Debug-build allocation detector for the audio thread.
//!
//! Rust has no effect system: a heap allocation inside `process()` cannot be
//! made a *compile* error. What it can be is a loud, immediate failure in
//! debug builds. [`RtAllocCheck`] wraps the system allocator; every
//! allocator call consults seco-core's thread-local real-time depth counter,
//! which the `RtContext` runner increments around each `process()` call.
//! Nonzero depth ⇒ write a static message to stderr and abort. Release
//! builds compile the check away, leaving a direct forward to the system
//! allocator.
//!
//! Registered automatically by [`seco_export!`](crate::seco_export) via
//! `#[global_allocator]`. Technique after Fritz Webering's
//! `assert_no_alloc`, reimplemented for SECO.

use std::alloc::{GlobalAlloc, Layout, System};

/// The wrapping allocator. Zero-sized; all state is the system allocator's.
pub struct RtAllocCheck;

#[cfg(debug_assertions)]
#[inline]
fn rt_check(message: &'static str) {
    use std::cell::Cell;
    thread_local! {
        // Std's stderr machinery may itself allocate on first use. While the
        // report is being written we are already committed to aborting, so
        // re-entrant allocator calls from the reporting path are let through
        // instead of recursing to a stack overflow.
        static REPORTING: Cell<bool> = const { Cell::new(false) };
    }
    if seco_core::__private::rt_depth() > 0 && !REPORTING.with(Cell::get) {
        REPORTING.with(|flag| flag.set(true));
        use std::io::Write;
        let _ = std::io::stderr().write_all(message.as_bytes());
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn rt_check(_message: &'static str) {}

// SAFETY: every operation forwards unchanged to `System`, which upholds the
// GlobalAlloc contract. The debug check neither allocates nor unwinds (it
// aborts the process), so the contract is preserved.
unsafe impl GlobalAlloc for RtAllocCheck {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        rt_check("seco: heap allocation inside the real-time audio callback\n");
        // SAFETY: same contract our caller gave us.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Freeing is just as non-real-time as allocating: free() may lock.
        rt_check("seco: heap deallocation inside the real-time audio callback\n");
        // SAFETY: same contract our caller gave us.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        rt_check("seco: heap allocation inside the real-time audio callback\n");
        // SAFETY: same contract our caller gave us.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        rt_check("seco: heap reallocation inside the real-time audio callback\n");
        // SAFETY: same contract our caller gave us.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

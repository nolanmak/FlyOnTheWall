//! Real-time safety enforcement.
//!
//! CAP-04: the Core Audio IOProc block may only copy bytes into a preallocated
//! ring and bump an atomic — no malloc, no locks, no I/O, no logging. This
//! module makes that assertable in CI rather than a code-review convention.
//!
//! Hand-rolled rather than depending on `assert_no_alloc`, which still works
//! but has not seen a release since 2021 — this is ~40 lines we can own
//! outright, and it is load-bearing enough to want to.
//!
//! # Wiring
//!
//! The detector only works if the crate installs [`RtAlloc`] as the global
//! allocator. `fotw-pipeline` does this for its own test binaries; a binary
//! that wants the check in production must opt in:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: fotw_pipeline::rt::RtAlloc = fotw_pipeline::rt::RtAlloc;
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

thread_local! {
    // `const` initialisation is not a micro-optimisation here, it is required:
    // a lazily-initialised thread_local allocates on first touch, which
    // re-enters the allocator we are hooking and either stack-overflows or
    // deadlocks. It would not show up in a trivial test and would blow up
    // under real load.
    static IN_RT: Cell<usize> = const { Cell::new(0) };
    static LOCAL_VIOLATIONS: Cell<usize> = const { Cell::new(0) };
}

static VIOLATIONS: AtomicUsize = AtomicUsize::new(0);

/// Allocations observed inside a [`RtGuard`] **on the calling thread**.
///
/// Per-thread rather than global on purpose. A violation is a property of one
/// real-time thread, and a global counter would make any two tests that use
/// [`RtGuard`] observe each other's allocations under `cargo test`'s default
/// parallelism — a flake that looks like a detector bug.
#[must_use]
pub fn allocation_violations() -> usize {
    LOCAL_VIOLATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Allocations observed inside a [`RtGuard`] across every thread.
///
/// This is the one to watch in a running daemon: the audio thread is not the
/// thread doing the watching.
#[must_use]
pub fn total_allocation_violations() -> usize {
    VIOLATIONS.load(Ordering::Relaxed)
}

/// Reset both counters. Test-support only.
pub fn reset_allocation_violations() {
    VIOLATIONS.store(0, Ordering::Relaxed);
    LOCAL_VIOLATIONS.try_with(|c| c.set(0)).ok();
}

/// True while the current thread is inside a real-time section.
#[must_use]
pub fn in_real_time_section() -> bool {
    // `try_with` rather than `with`: during thread teardown the thread_local
    // is already destroyed and `with` panics — inside an allocator that would
    // turn a clean exit into an abort.
    IN_RT.try_with(|c| c.get() > 0).unwrap_or(false)
}

/// Marks a scope as real-time. Allocations inside it are counted as
/// violations. Nests correctly.
#[derive(Debug)]
pub struct RtGuard {
    _private: (),
}

impl RtGuard {
    /// Enter a real-time section for the current thread.
    #[must_use]
    pub fn enter() -> Self {
        IN_RT.with(|c| c.set(c.get() + 1));
        Self { _private: () }
    }
}

impl Drop for RtGuard {
    fn drop(&mut self) {
        IN_RT.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// A global allocator that counts allocations made on a real-time thread.
///
/// Delegates every allocation to the system allocator — it never changes
/// behaviour, only observes it, so a violation degrades performance rather
/// than correctness in a shipping build.
#[derive(Debug, Clone, Copy, Default)]
pub struct RtAlloc;

impl RtAlloc {
    #[inline]
    fn record() {
        if in_real_time_section() {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
            LOCAL_VIOLATIONS.try_with(|c| c.set(c.get() + 1)).ok();
            // Deliberately a raw write of a fixed byte string: formatting or
            // println! here would allocate, re-enter this allocator and either
            // deadlock on the stdout lock or overflow the stack. The bug would
            // appear only once a violation actually occurs — precisely when
            // the tool is needed.
            const MSG: &[u8] = b"fotw: allocation on the real-time audio path\n";
            // SAFETY: write(2) to stderr with a valid pointer and length. Not
            // async-signal-unsafe and does not allocate.
            unsafe {
                libc_write(2, MSG.as_ptr(), MSG.len());
            }
        }
    }
}

// Declared locally so the crate does not take a `libc` dependency for one call.
unsafe extern "C" {
    #[link_name = "write"]
    fn libc_write(fd: i32, buf: *const u8, count: usize) -> isize;
}

unsafe impl GlobalAlloc for RtAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Deallocation can also block in a real allocator, but counting it
        // would flag every Drop of something merely *created* outside the
        // section, which is noise. Allocation is the signal that matters.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::record();
        unsafe { System.alloc_zeroed(layout) }
    }
}

//! The real-time contract, asserted rather than hoped for.
//!
//! The Core Audio IOProc block runs on a real-time thread. It may only memcpy
//! into a preallocated ring and bump an atomic: no malloc, no locks, no I/O,
//! no logging (docs/REQUIREMENTS.md CAP-04). Rust can enforce that in CI,
//! which is the single concrete advantage this stack has over Swift here,
//! where ARC traffic on the same path is a matter of discipline.

use fotw_pipeline::rt::{RtAlloc, RtGuard, allocation_violations};

// The detector is only live if THIS binary installs it. A `#[cfg(test)]`
// global allocator inside the library does not apply here: an integration test
// links the library compiled *without* `cfg(test)`, so the hook is absent and
// every assertion below silently passes for the wrong reason. That failure
// mode is invisible — the tests go green while checking nothing — which is
// why the crate exports `RtAlloc` and expects callers to wire it up.
#[global_allocator]
static ALLOC: RtAlloc = RtAlloc;

#[test]
fn a_clean_real_time_section_records_no_violations() {
    let before = allocation_violations();
    let mut scratch = vec![0.0f32; 1024];

    {
        let _rt = RtGuard::enter();
        // Only writes into already-allocated memory.
        for (i, s) in scratch.iter_mut().enumerate() {
            *s = i as f32;
        }
    }

    assert_eq!(
        allocation_violations(),
        before,
        "writing into preallocated memory must not allocate"
    );
    assert_eq!(scratch[1023], 1023.0);
}

#[test]
fn an_allocation_inside_a_real_time_section_is_detected() {
    let before = allocation_violations();

    {
        let _rt = RtGuard::enter();
        // Exactly the kind of thing that must never appear on the audio path.
        let leaked: Vec<f32> = Vec::with_capacity(512);
        std::hint::black_box(&leaked);
    }

    assert!(
        allocation_violations() > before,
        "an allocation inside RtGuard must be counted; if this fails the \
         detector is not wired up and every other no-alloc test is vacuous"
    );
}

#[test]
fn the_guard_is_scoped_and_nests() {
    let before = allocation_violations();
    {
        let _outer = RtGuard::enter();
        {
            let _inner = RtGuard::enter();
        }
        // Still inside the outer guard: the inner drop must not have cleared it.
        let v: Vec<u8> = Vec::with_capacity(8);
        std::hint::black_box(&v);
    }
    let during = allocation_violations();
    assert!(during > before);

    // Outside every guard, allocation is ordinary and uncounted.
    let v: Vec<u8> = Vec::with_capacity(8);
    std::hint::black_box(&v);
    assert_eq!(allocation_violations(), during);
}

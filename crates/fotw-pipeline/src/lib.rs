//! `fotw-pipeline` — everything between the capture seam and the network.
//!
//! Rings, resampling, echo cancellation, the write-ahead log, and the
//! backpressure state machine. The invariant the whole crate exists to
//! protect: **audio-to-disk is the crash invariant, and the transcript is
//! derived.** The network is the only backpressure point, and it must never
//! stall the audio thread (docs/REQUIREMENTS.md 5.2, 5.3).

#![warn(missing_docs)]

pub mod echo;
pub mod opus;
pub mod promote;
pub mod resample;
pub mod retention;
pub mod ring;
pub mod rt;
pub mod wal;

// NOTE: deliberately no `#[cfg(test)] #[global_allocator]` here. It would
// cover this crate's unit tests but NOT its integration tests, which link the
// library built without `cfg(test)` — so the real-time assertions would pass
// while checking nothing. Each test binary installs `rt::RtAlloc` itself.

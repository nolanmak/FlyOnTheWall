//! `fotw-stt` — the speech-to-text provider abstraction.
//!
//! This crate owns the canonical internal transcript format (spec 7.2) that
//! every provider adapter normalizes into, the capability descriptor the UI
//! reads to hide unsupported affordances (spec 7.3, STT-02), the shared error
//! taxonomy (STT-12), and the provider response normalizers.
//!
//! It is deliberately pure data and logic: no sockets, no async runtime, no
//! clock reads. Everything that touches the network lives above it, which is
//! what makes the normalization rules testable from checked-in fixtures.
//!
//! See `docs/REQUIREMENTS.md` §7.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clock;
pub mod speaker;
pub mod transcript;

pub use clock::{ArrivalEstimator, SessionClock, TimeSpan, seconds_to_ms};
pub use speaker::{SpeakerNormalizer, SpeakerRegistry};
pub use transcript::{Source, TimestampSource, TranscriptSegment, Word};

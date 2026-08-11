//! `fotwd` — the FlyOnTheWall daemon.
//!
//! Owns capture, transcription and storage, and (later) serves the web UI on
//! loopback. Exposed as a library so the same session machinery is reachable
//! from tests and from the `fotw` CLI without duplicating the wiring.

#![warn(missing_docs)]

pub mod consent;
pub mod persist;
pub mod secrets;
pub mod session;
pub mod summarize;
pub mod transport;

pub use session::{SessionOutcome, Transcription};

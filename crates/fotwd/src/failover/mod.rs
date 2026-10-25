//! Durable cloud-to-local transcription handoff for meeting capture.
//!
//! The controller owns per-source audio coverage and provider epochs. Disk IO
//! and native inference belong on its blocking worker, independently of capture.

pub mod coverage;
pub mod policy;
pub mod stream;

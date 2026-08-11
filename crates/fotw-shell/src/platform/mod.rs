//! The per-OS shell backend.
//!
//! macOS gets a real AppKit shell; every other target gets [`stub`], which
//! refuses to start with a typed error rather than pretending. CI builds both
//! (docs/REQUIREMENTS.md 5.6), which is the only thing that keeps the
//! headless core from quietly growing a macOS-shaped dependency.

pub mod stub;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use crate::platform::macos::{probe, run};

#[cfg(not(target_os = "macos"))]
pub use crate::platform::stub::{probe, run};

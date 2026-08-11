//! Per-OS backends behind the seam.
//!
//! Real capture is cfg-gated so nothing macOS-specific compiles on Linux, and
//! CI cross-checks this crate against `x86_64-pc-windows-msvc` against a stub.
//! Building the seam before any capture code costs a few days; retrofitting it
//! later costs weeks plus macOS regression risk (docs/REQUIREMENTS.md 6.5).

pub mod file;
pub mod stub;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

pub use crate::platform::stub::StubPlatform;

/// The backend for the OS this binary was built for.
///
/// Today every OS resolves to [`StubPlatform`], which refuses to open anything
/// with a typed error. Real capture lands per-platform behind this function,
/// so callers never gain an `#[cfg]`.
#[must_use]
pub fn host() -> StubPlatform {
    StubPlatform::new()
}

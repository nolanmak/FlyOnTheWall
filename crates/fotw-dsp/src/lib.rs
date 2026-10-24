//! Shared PCM conversion without capture, storage or model dependencies.
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
pub mod resample;

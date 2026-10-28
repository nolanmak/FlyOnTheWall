//! Private IPC and owned process lifetime for local summaries.
//!
//! This is not a connector to an arbitrary localhost daemon. The caller must
//! own the child; every connected socket is authenticated against its PID
//! before any request bytes are sent. No TCP/DNS connector is constructed.

#[cfg(target_os = "macos")]
mod adapter;
#[cfg(any(target_os = "macos", test))]
mod ipc;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
pub use adapter::LocalAdapter;
#[cfg(target_os = "macos")]
pub use process::{Artifact, RuntimeConfig};

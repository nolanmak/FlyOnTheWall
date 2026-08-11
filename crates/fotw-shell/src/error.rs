//! Errors the shell can report.
//!
//! Every variant carries a `String` rather than a platform error type. No
//! Objective-C or Core Graphics type may appear in this crate's public API,
//! for the same reason `fotw-audio` forbids it: the moment one does, every
//! caller above the seam grows an `#[cfg]` (docs/REQUIREMENTS.md 6.5).

use crate::hotkey::{Chord, HotkeyError};

/// Why the shell could not start, or could not do something it was asked to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ShellError {
    /// This build has no AppKit shell.
    #[error("the AppKit shell requires macOS; this binary targets {os}")]
    Unsupported {
        /// The target OS this binary was built for.
        os: &'static str,
    },

    /// AppKit was touched from somewhere other than the main thread.
    ///
    /// `NSApplication`, `NSStatusItem` and `NSPanel` are all main-thread-only.
    /// `tray-icon` reports this as `Error::NotMainThread`; we surface it with
    /// the same meaning rather than letting a `MainThreadMarker` unwrap abort.
    #[error("the shell must be created on the main thread")]
    NotMainThread,

    /// The menu-bar item could not be created or retained.
    #[error("menu-bar item: {0}")]
    StatusItem(String),

    /// The recording pill could not be built.
    #[error("recording pill: {0}")]
    Pill(String),

    /// A hotkey could not be registered with the OS.
    #[error("hotkey {chord}: {reason}")]
    HotkeyRegistration {
        /// The chord that failed.
        chord: Chord,
        /// What the OS or `global-hotkey` said.
        reason: String,
    },

    /// A hotkey was rejected before it reached the OS.
    #[error(transparent)]
    Hotkey(#[from] HotkeyError),
}

impl ShellError {
    /// The `Unsupported` error for the current target.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self::Unsupported {
            os: std::env::consts::OS,
        }
    }
}

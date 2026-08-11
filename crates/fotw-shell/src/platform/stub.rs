//! The backend for targets with no AppKit shell.
//!
//! Refuses to start rather than running a shell with no recording indicator.
//! CON-02 makes the indicator a P0 product requirement, so "run anyway,
//! without it" is not an available degraded mode
//! (docs/REQUIREMENTS.md 11.2).

use std::convert::Infallible;

use crate::error::ShellError;
use crate::hotkey::HotkeyMap;
use crate::probe::ShellProbe;
use crate::runtime::ShellHost;

/// Always fails with [`ShellError::Unsupported`].
///
/// # Errors
///
/// Always.
pub fn run<H: ShellHost>(_host: H, _hotkeys: HotkeyMap) -> Result<Infallible, ShellError> {
    Err(ShellError::unsupported())
}

/// Always fails with [`ShellError::Unsupported`].
///
/// # Errors
///
/// Always.
pub fn probe() -> Result<ShellProbe, ShellError> {
    Err(ShellError::unsupported())
}

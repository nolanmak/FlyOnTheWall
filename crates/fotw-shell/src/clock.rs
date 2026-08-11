//! A monotonic time reading the state machine can be driven with.
//!
//! [`ShellCore`](crate::ShellCore) never calls `Instant::now()`. Every input
//! that needs a clock carries one, so the whole state machine is a pure
//! function of its input sequence and CI can exercise a two-hour meeting in
//! microseconds. The AppKit layer is the only thing that reads a real clock.
//!
//! `Instant` is deliberately *not* the type used here: `Instant` cannot be
//! constructed at an arbitrary point in a test, and `Instant - Instant`
//! panics on a backwards reading. [`Monotonic::since`] saturates instead.

use std::time::Duration;

/// A reading from a monotonic clock: elapsed time since an arbitrary epoch.
///
/// Only differences between two readings are meaningful.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Monotonic(Duration);

impl Monotonic {
    /// The epoch itself.
    pub const ZERO: Self = Self(Duration::ZERO);

    /// A reading `secs` seconds after the epoch.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// A reading `millis` milliseconds after the epoch.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(Duration::from_millis(millis))
    }

    /// A reading built from a raw [`Duration`] since the epoch.
    #[must_use]
    pub const fn from_duration(since_epoch: Duration) -> Self {
        Self(since_epoch)
    }

    /// This reading as a [`Duration`] since the epoch.
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Time from `earlier` to `self`, **saturating at zero**.
    ///
    /// A clock that appears to run backwards is a real occurrence (a coarse
    /// timer source, a suspended process, a test that replays out of order).
    /// The recording indicator must not panic because of one, and elapsed
    /// time must never appear to shrink, so this clamps rather than wrapping
    /// or aborting.
    #[must_use]
    pub fn since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// This reading advanced by `delta`.
    #[must_use]
    pub fn plus(self, delta: Duration) -> Self {
        Self(self.0.saturating_add(delta))
    }
}

/// Render an elapsed duration the way the pill and the menu bar show it.
///
/// `MM:SS` below an hour, `H:MM:SS` at or above one. Minutes and seconds are
/// always two digits so the label does not change width mid-meeting (a pill
/// that reflows every ten seconds is visually noisy in a call).
#[must_use]
pub fn format_elapsed(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

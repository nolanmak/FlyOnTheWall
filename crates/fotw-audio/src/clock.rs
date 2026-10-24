//! The one host clock every tap stamps from (seam rule 3).
//!
//! macOS capture uses a process-relative continuous clock that includes sleep.
//! Other platforms retain their monotonic `Instant` clock. Native clock types
//! stay below the platform boundary; taps and supervisors use [`host_ns`].
//!
//! # Why there is a trait as well as a function
//!
//! The stall watchdog's thresholds are 5 and 8 seconds. Testing it against
//! [`host_ns`] would mean a test suite that sleeps, and a 13-second test is a
//! test nobody runs. [`Clock`] is the injection point: production passes
//! [`HostClock`], tests pass `testing::ManualClock`, and the detection logic
//! itself never reads a clock at all — it takes `now_ns` as an argument.

use std::sync::OnceLock;
use std::time::Instant;

/// A source of monotonic nanoseconds.
///
/// `Send + Sync` because the supervisor holds one behind an `Arc` and is
/// driven from a different thread than the one that built it.
pub trait Clock: Send + Sync {
    /// Nanoseconds on this clock. Must be monotonically non-decreasing.
    fn now_ns(&self) -> u64;
}

/// The real clock: the process-wide monotonic host clock of [`host_ns`].
#[derive(Debug, Clone, Copy, Default)]
pub struct HostClock;

impl Clock for HostClock {
    fn now_ns(&self) -> u64 {
        host_ns()
    }
}

/// The legacy Instant stopwatch epoch. On macOS this is not the capture
/// epoch; it remains available for callers measuring awake-time intervals.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Nanoseconds on the process-wide monotonic host clock.
///
/// Monotonically non-decreasing for the life of the process. The absolute
/// value is meaningless on its own; only differences between two readings are.
#[must_use]
pub fn host_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        crate::platform::macos::capture_clock::host_ns()
    }
    #[cfg(not(target_os = "macos"))]
    {
        ns_since(*EPOCH.get_or_init(Instant::now))
    }
}

/// Nanoseconds elapsed on an Instant stopwatch since `origin`, saturating.
/// This compatibility helper excludes macOS sleep. For capture elapsed time,
/// subtract two [`host_ns`] readings instead.
#[must_use]
pub fn ns_since(origin: Instant) -> u64 {
    u64::try_from(Instant::now().saturating_duration_since(origin).as_nanos()).unwrap_or(u64::MAX)
}

/// The legacy Instant stopwatch epoch, initialized on first use.
/// This is not the macOS capture epoch and must not convert native capture
/// timestamps. Use [`host_ns`] for the shared capture timeline.
#[must_use]
pub fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ns_is_monotonic() {
        let mut last = host_ns();
        for _ in 0..1_000 {
            let now = host_ns();
            assert!(now >= last, "{now} < {last}");
            last = now;
        }
    }

    #[test]
    fn the_epoch_is_stable_across_calls() {
        assert_eq!(epoch(), epoch());
    }
}

//! Sleep-aware process-relative capture timestamps.
use std::num::NonZeroU32;

use std::sync::OnceLock;

struct Timebase {
    origin: u64,
    numer: u32,
    denom: NonZeroU32,
}
static TIMEBASE: OnceLock<Timebase> = OnceLock::new();
fn timebase() -> &'static Timebase {
    TIMEBASE.get_or_init(|| {
        let info = cidre::mach::TimeBaseInfo::new();
        assert!(info.numer > 0, "invalid capture clock timebase numerator");
        Timebase {
            origin: cidre::mach::continuous_time(),
            numer: info.numer,
            denom: NonZeroU32::new(info.denom).expect("invalid capture clock timebase denominator"),
        }
    })
}

pub(crate) fn host_ns() -> u64 {
    let base = timebase();
    elapsed_ns(
        base.origin,
        cidre::mach::continuous_time(),
        base.numer,
        base.denom,
    )
}

fn elapsed_ns(origin: u64, now: u64, numer: u32, denom: NonZeroU32) -> u64 {
    let scaled =
        u128::from(now.saturating_sub(origin)) * u128::from(numer) / u128::from(denom.get());
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_capture_clock_uses_the_native_continuous_epoch() {
        let base = timebase();
        let before = elapsed_ns(
            base.origin,
            cidre::mach::continuous_time(),
            base.numer,
            base.denom,
        );
        let actual = crate::clock::host_ns();
        let after = elapsed_ns(
            base.origin,
            cidre::mach::continuous_time(),
            base.numer,
            base.denom,
        );
        assert!(before <= actual && actual <= after);
        assert!(std::ptr::eq(base, timebase()), "all taps share one epoch");
    }
    #[test]
    fn tick_conversion_preserves_sleep_elapsed_and_fractional_timebases() {
        let denom = NonZeroU32::new(3).unwrap();
        let origin = 1_000_000;
        assert_eq!(
            elapsed_ns(origin, origin + 9_000_000_000, 100, denom),
            300_000_000_000
        );
        assert_eq!(elapsed_ns(origin, origin + 25, 1, denom), 8);
    }
    #[test]
    fn conversion_avoids_intermediate_overflow_and_saturates_only_at_the_end() {
        let denom = NonZeroU32::new(u32::MAX).unwrap();
        assert_eq!(elapsed_ns(0, u64::MAX, u32::MAX, denom), u64::MAX);
        assert_eq!(
            elapsed_ns(0, u64::MAX, 2, NonZeroU32::new(1).unwrap()),
            u64::MAX
        );
        assert_eq!(elapsed_ns(100, 99, 100, NonZeroU32::new(3).unwrap()), 0);
    }
}

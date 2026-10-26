//! Sleep-aware process-relative capture timestamps.
use std::num::NonZeroU32;

use std::sync::OnceLock;

use crate::error::TapError;
use crate::frames::{CaptureTimestamp, FrameFlags, FrameSink};
use cidre::cat;

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

// A bridge wider than 100us cannot qualify input timing. Audio still passes
// through with TIMESTAMP_ERROR. Native input older than 1s is likewise stale.
const MAX_BRIDGE_NS: u64 = 100_000;
const MAX_INPUT_AGE_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy)]
struct ClockReading {
    before: u64,
    continuous: u64,
    after: u64,
}
impl ClockReading {
    fn now() -> Self {
        let before = cidre::mach::abs_time();
        let continuous = cidre::mach::continuous_time();
        let after = cidre::mach::abs_time();
        Self {
            before,
            continuous,
            after,
        }
    }
    fn offset(self, base: &Timebase) -> Option<i128> {
        let width = self.after.checked_sub(self.before)?;
        if elapsed_ns(0, width, base.numer, base.denom) > MAX_BRIDGE_NS {
            return None;
        }
        Some(i128::from(self.continuous) - i128::from(self.before + width / 2))
    }
}

#[derive(Default)]
struct InputClock {
    offset: Option<i128>,
    expected_sample: Option<f64>,
    last_host: Option<u64>,
}
impl InputClock {
    fn stamp(
        &mut self,
        native: &cat::AudioTimeStamp,
        delivered: u64,
        frames: u64,
        reading: ClockReading,
        base: &Timebase,
    ) -> (CaptureTimestamp, FrameFlags) {
        let mut flags = FrameFlags::empty();
        let offset = reading.offset(base);
        let shifted = offset.zip(self.offset).is_some_and(|(now, last)| {
            let delta = u64::try_from((now - last).unsigned_abs()).unwrap_or(u64::MAX);
            elapsed_ns(0, delta, base.numer, base.denom) > MAX_BRIDGE_NS
        });
        if let Some(offset) = offset {
            self.offset = Some(offset);
        }
        if shifted {
            flags |= FrameFlags::DISCONTINUITY | FrameFlags::TIMESTAMP_ERROR;
        }
        // Stay within the exact integer range of f64, including the next frame.
        let sample_valid = native.flags.0 & cat::AudioTimeStampFlags::SAMPLE_TIME_VALID.0 != 0
            && native.sample_time.is_finite()
            && native.sample_time.fract() == 0.0
            && native.sample_time.abs() + (frames as f64) < (1_u64 << 53) as f64;
        if sample_valid {
            if let Some(expected) = self.expected_sample
                && native.sample_time != expected
            {
                flags |= FrameFlags::DISCONTINUITY;
                if native.sample_time < expected {
                    flags |= FrameFlags::TIMESTAMP_ERROR;
                }
            }
            self.expected_sample = Some(native.sample_time + frames as f64);
        } else {
            self.expected_sample = None;
            flags |= FrameFlags::TIMESTAMP_ERROR;
        }
        let mapped = (|| {
            if !sample_valid
                || flags.contains(FrameFlags::TIMESTAMP_ERROR)
                || native.flags.0 & cat::AudioTimeStampFlags::HOST_TIME_VALID.0 == 0
            {
                return None;
            }
            let age = reading.before.checked_sub(native.host_time)?;
            if elapsed_ns(0, age, base.numer, base.denom) > MAX_INPUT_AGE_NS {
                return None;
            }
            let continuous = u64::try_from(i128::from(native.host_time) + offset?).ok()?;
            let delta = continuous.checked_sub(base.origin)?;
            let ns = u128::from(delta) * u128::from(base.numer) / u128::from(base.denom.get());
            u64::try_from(ns).ok()
        })();
        let fallback = elapsed_ns(base.origin, reading.continuous, base.numer, base.denom);
        let host = match mapped {
            Some(host) if self.last_host.is_none_or(|last| host > last) => host,
            _ => {
                flags |= FrameFlags::TIMESTAMP_ERROR;
                if mapped.is_some() {
                    flags |= FrameFlags::DISCONTINUITY;
                }
                self.last_host.map_or(fallback, |last| fallback.max(last))
            }
        };
        self.last_host = Some(host);
        (CaptureTimestamp::new(delivered, host), flags)
    }
}

/// Timing state and application sink share the callback's existing try-lock.
/// Construction (including epoch initialization) happens on the control thread.
pub(super) struct CaptureSink {
    sink: Box<dyn FrameSink>,
    clock: InputClock,
}
impl CaptureSink {
    pub(super) fn new(sink: Box<dyn FrameSink>) -> Self {
        let base = timebase();
        let offset = ClockReading::now().offset(base);
        Self {
            sink,
            clock: InputClock {
                offset,
                ..InputClock::default()
            },
        }
    }
    pub(super) fn on_input(
        &mut self,
        pcm: &[f32],
        native: &cat::AudioTimeStamp,
        delivered: u64,
        frames: u64,
        flags: FrameFlags,
    ) {
        let (ts, timing_flags) =
            self.clock
                .stamp(native, delivered, frames, ClockReading::now(), timebase());
        self.sink.on_frames(pcm, ts, flags | timing_flags);
    }
    pub(super) fn on_error(&mut self, error: TapError) {
        self.sink.on_error(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::FrameFlags;
    use cidre::cat;

    fn fixture_base() -> Timebase {
        Timebase {
            origin: 1000,
            numer: 1,
            denom: NonZeroU32::new(1).unwrap(),
        }
    }
    fn reading(absolute: u64, offset: u64) -> ClockReading {
        ClockReading {
            before: absolute,
            continuous: absolute + offset,
            after: absolute,
        }
    }
    #[test]
    fn native_input_time_removes_delivery_jitter_and_detects_sample_gaps() {
        let base = fixture_base();
        let mut clock = InputClock::default();
        let stamp = cat::AudioTimeStamp::with_sample_and_host_time(0.0, 9000);
        let (ts, flags) = clock.stamp(&stamp, 0, 480, reading(10000, 1000), &base);
        assert_eq!((ts.device_frames, ts.host_ns), (0, 9000));
        assert!(flags.is_empty());
        let stamp = cat::AudioTimeStamp::with_sample_and_host_time(480.0, 10000);
        let (ts, flags) = clock.stamp(&stamp, 480, 480, reading(50000, 1000), &base);
        assert_eq!((ts.device_frames, ts.host_ns), (480, 10000));
        assert!(
            flags.is_empty(),
            "callback jitter is not a source clock gap"
        );
        let stamp = cat::AudioTimeStamp::with_sample_and_host_time(1440.0, 11000);
        let (_, flags) = clock.stamp(&stamp, 960, 480, reading(51000, 1000), &base);
        assert!(flags.contains(FrameFlags::DISCONTINUITY));
    }
    #[test]
    fn native_projection_preserves_zero_and_fractional_timebases_but_rejects_pre_epoch() {
        let mut base = fixture_base();
        let zero = cat::AudioTimeStamp::with_sample_and_host_time(0.0, 0);
        let (ts, flags) = InputClock::default().stamp(&zero, 0, 1, reading(10000, 1000), &base);
        assert_eq!(ts.host_ns, 0);
        assert!(flags.is_empty());
        base.origin = 2000;
        let (ts, flags) = InputClock::default().stamp(&zero, 0, 1, reading(10000, 1000), &base);
        assert_eq!(ts.host_ns, 9000);
        assert!(flags.contains(FrameFlags::TIMESTAMP_ERROR));
        base.origin = 1000;
        base.numer = 100;
        base.denom = NonZeroU32::new(3).unwrap();
        let native = cat::AudioTimeStamp::with_sample_and_host_time(-480.0, 9000);
        let (ts, flags) = InputClock::default().stamp(&native, 0, 480, reading(10000, 1000), &base);
        assert_eq!(ts.host_ns, 300_000);
        assert!(
            flags.is_empty(),
            "valid negative native sample origins are allowed"
        );
    }
    #[test]
    fn invalid_native_time_falls_back_without_moving_backwards() {
        for native in [
            cat::AudioTimeStamp::invalid(),
            cat::AudioTimeStamp::with_sample_and_host_time(f64::NAN, 9000),
            cat::AudioTimeStamp::with_sample_and_host_time(0.5, 9000),
            cat::AudioTimeStamp::with_sample_and_host_time(0.0, 10001),
        ] {
            let mut clock = InputClock::default();
            let (ts, flags) = clock.stamp(&native, 0, 480, reading(10000, 1000), &fixture_base());
            assert_eq!(ts.host_ns, 10000);
            assert!(flags.contains(FrameFlags::TIMESTAMP_ERROR));
        }
        let mut clock = InputClock::default();
        clock.stamp(
            &cat::AudioTimeStamp::invalid(),
            0,
            480,
            reading(10000, 1000),
            &fixture_base(),
        );
        let (ts, flags) = clock.stamp(
            &cat::AudioTimeStamp::with_sample_and_host_time(0.0, 9000),
            480,
            480,
            reading(11000, 1000),
            &fixture_base(),
        );
        assert_eq!(ts.host_ns, 11000);
        assert!(flags.contains(FrameFlags::TIMESTAMP_ERROR | FrameFlags::DISCONTINUITY));
    }
    #[test]
    fn sleep_epoch_change_and_uncertain_bridge_are_not_trusted() {
        let base = fixture_base();
        let mut clock = InputClock::default();
        clock.stamp(
            &cat::AudioTimeStamp::with_sample_and_host_time(0.0, 9000),
            0,
            480,
            reading(10000, 1000),
            &base,
        );
        let (ts, flags) = clock.stamp(
            &cat::AudioTimeStamp::with_sample_and_host_time(480.0, 10000),
            480,
            480,
            reading(11000, 10_001_000),
            &base,
        );
        assert_eq!(ts.host_ns, 10_011_000);
        assert!(flags.contains(FrameFlags::TIMESTAMP_ERROR | FrameFlags::DISCONTINUITY));
        let (_, flags) = clock.stamp(
            &cat::AudioTimeStamp::with_sample_and_host_time(960.0, 12000),
            960,
            480,
            reading(13000, 10_001_000),
            &base,
        );
        assert!(
            flags.is_empty(),
            "new stable bridge can recover after the fault"
        );
        let uncertain = ClockReading {
            before: 20000,
            continuous: 21000,
            after: 220001,
        };
        let (_, flags) = InputClock::default().stamp(
            &cat::AudioTimeStamp::with_sample_and_host_time(0.0, 19000),
            0,
            480,
            uncertain,
            &base,
        );
        assert!(flags.contains(FrameFlags::TIMESTAMP_ERROR));
        let (_, flags) = InputClock::default().stamp(
            &cat::AudioTimeStamp::with_sample_and_host_time(0.0, 1000),
            0,
            480,
            reading(2_000_000_000, 1000),
            &base,
        );
        assert!(
            flags.contains(FrameFlags::TIMESTAMP_ERROR),
            "stale native time is not current audio timing"
        );
    }
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

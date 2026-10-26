//! Recording-side AEC coordination. Native clock alignment is independent of ASR.
mod coordinator;
mod request;
pub use coordinator::Coordinator;
use fotw_pipeline::{
    reference_clock::{Estimate, Rejection, Tracker},
    reference_resample::{FrameDomain, NativeFrame, ProcessingFrame},
    resample::ConversionOrigin,
    ring::CaptureObservation,
};
pub(crate) use request::Prepared;
pub use request::Processing;

/// Actual conversion geometry for one microphone tap generation. Archive frame
/// positions include earlier generations and gap padding; canonical positions
/// additionally include the source's original session anchor.
#[derive(Clone, Copy, Debug)]
pub struct ConversionChain {
    /// Native capture rate for this generation.
    pub native_rate_hz: u32,
    /// Fixed rate of this recording's microphone archive.
    pub archive_rate_hz: u32,
    /// Native-frame residual of the native-to-archive converter.
    pub archive_phase: ConversionOrigin,
    /// Archive-frame residual of the archive-to-canonical converter.
    pub canonical_phase: ConversionOrigin,
    /// First archive frame owned by this generation, after earlier gap padding.
    pub archive_start_frame: u64,
    /// Original absolute source anchor in mono 16kHz samples.
    pub canonical_anchor: ProcessingFrame,
    /// Accepted native-rate frame corresponding to this converter's input zero.
    /// This is not the device-frame counter used to detect capture faults.
    pub native_start_frame: NativeFrame,
}
impl ConversionChain {
    /// Bind the two actual converter origins to an absolute processing domain.
    /// Invalid/overflowing geometry must cause original-microphone passthrough.
    pub fn domain(self) -> Result<FrameDomain, &'static str> {
        if !(4000..=768000).contains(&self.native_rate_hz)
            || !(4000..=768000).contains(&self.archive_rate_hz)
            || self.archive_phase.denominator == 0
            || self.canonical_phase.denominator == 0
        {
            return Err("invalid microphone conversion geometry");
        }
        let phase = |origin: ConversionOrigin| {
            origin.input_frame_numerator as f64 / origin.denominator as f64
        };
        let archive_phase = phase(self.archive_phase);
        let canonical_phase = phase(self.canonical_phase);
        if archive_phase.abs() > 256.0 || canonical_phase.abs() > 256.0 {
            return Err("unsupported converter phase");
        }
        // Keep absolute counters integer. Converting an entire long-session
        // position to f64 would erase the fractional generation boundary.
        let scaled = u128::from(self.archive_start_frame) * 16000;
        let rate = u128::from(self.archive_rate_hz);
        let processing = u64::try_from(scaled / rate).map_err(|_| "processing frame overflow")?;
        let processing = self
            .canonical_anchor
            .0
            .checked_add(processing)
            .ok_or("source clock overflow")?;
        let remainder = (scaled % rate) as f64 / 16000.0;
        let offset = (canonical_phase - remainder) * f64::from(self.native_rate_hz)
            / f64::from(self.archive_rate_hz)
            + archive_phase;
        if !offset.is_finite() || offset.abs() > 256.0 {
            return Err("unsupported composed converter phase");
        }
        Ok(FrameDomain {
            native_origin: self.native_start_frame,
            native_offset_frames: offset,
            processing_origin: ProcessingFrame(processing),
            native_rate_hz: self.native_rate_hz,
        })
    }
}

/// A capture owner's timing state for one ring/tap generation. Observations
/// describe accepted interleaved sample positions, not arbitrary drain times.
pub struct SourceClock {
    tracker: Tracker,
    generation: u64,
    channels: u16,
    latest: Option<CaptureObservation>,
    fence: u64,
    estimate: Option<Estimate>,
}
impl SourceClock {
    /// Preallocate rate-fitting state on the pump/control thread.
    pub fn new(rate_hz: u32, channels: u16, generation: u64) -> Result<Self, Rejection> {
        Ok(Self {
            tracker: Tracker::new(rate_hz, channels)?,
            generation,
            channels,
            latest: None,
            fence: 0,
            estimate: None,
        })
    }
    /// A timing refusal fences earlier queued input as well as cached fits.
    /// The owner must also invalidate aligned reference/native worker history.
    pub fn observe(
        &mut self,
        observation: CaptureObservation,
        now_ns: u64,
    ) -> Result<Option<Estimate>, Rejection> {
        if self.latest.is_none() {
            self.fence = self.fence.max(observation.sample_start);
        }
        match self.tracker.observe(self.generation, observation, now_ns) {
            Ok(estimate) => {
                self.latest = Some(observation);
                self.estimate = estimate;
                Ok(estimate)
            }
            Err(error) => {
                self.fence = self.fence.max(
                    observation
                        .sample_start
                        .saturating_add(observation.sample_count as u64),
                );
                self.latest = None;
                self.estimate = None;
                Err(error)
            }
        }
    }
    /// Current qualified clock, provided its original observation is still fresh.
    pub fn estimate(&self, now_ns: u64) -> Option<Estimate> {
        self.estimate.filter(|estimate| estimate.is_fresh(now_ns))
    }
    /// Resolve a drained interleaved range onto the accepted native-rate clock
    /// used by Tracker/Estimate and reference history. Device frame counters
    /// validate continuity; rejected ring samples must not enter this index.
    /// Untimed, partial-frame, stale, unobserved or fault-crossing ranges refuse.
    pub fn native_start(
        &self,
        start_sample: u64,
        samples: usize,
        now_ns: u64,
    ) -> Option<NativeFrame> {
        let latest = self.latest?;
        let channels = u64::from(self.channels);
        if samples == 0
            || !start_sample.is_multiple_of(channels)
            || !(samples as u64).is_multiple_of(channels)
            || start_sample < self.fence
            || now_ns.checked_sub(latest.host_ns)? > 1_000_000_000
            || start_sample.checked_add(samples as u64)?
                > latest
                    .sample_start
                    .checked_add(latest.sample_count as u64)?
        {
            return None;
        }
        Some(NativeFrame(start_sample / channels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_pipeline::resample::ArchiveConverter;

    #[test]
    fn actual_two_stage_pcm_keeps_native_phase_after_an_earlier_generation_and_gap() {
        for (native_rate, archive_rate) in [(44100, 48000), (48000, 44100), (96000, 48000)] {
            let mut archive = ArchiveConverter::new(native_rate, 1, archive_rate, 1).unwrap();
            let mut canonical = ArchiveConverter::new(archive_rate, 1, 16000, 1).unwrap();
            let prefix = 12346;
            let chain = ConversionChain {
                native_rate_hz: native_rate,
                archive_rate_hz: archive_rate,
                archive_phase: archive.output_origin(),
                canonical_phase: canonical.output_origin(),
                archive_start_frame: prefix,
                canonical_anchor: ProcessingFrame(137 * 16),
                native_start_frame: NativeFrame(0),
            };
            let signal = |frame: f64| {
                0.2 * (frame / f64::from(native_rate) * 1000.0 * std::f64::consts::TAU).sin()
            };
            let input: Vec<_> = (0..native_rate * 3)
                .map(|i| signal(f64::from(i)) as f32)
                .collect();
            let mut fixed = vec![0.0; prefix as usize];
            // Exercise arbitrary callback boundaries, including FFT tails.
            for chunk in input.chunks(997) {
                fixed.extend(archive.process(chunk).unwrap());
            }
            fixed.extend(archive.finish().unwrap());
            let mut output = Vec::new();
            for chunk in fixed.chunks(613) {
                output.extend(canonical.process(chunk).unwrap());
            }
            output.extend(canonical.finish().unwrap());
            let domain = chain.domain().unwrap();
            let begin = prefix as usize * 16000 / archive_rate as usize + 1000;
            let end = output.len() - 1000;
            let mut squared = 0.0;
            for (i, &sample) in output.iter().enumerate().take(end).skip(begin) {
                let absolute = 137 * 16 + i as u64;
                let native = (absolute - domain.processing_origin.0) as f64
                    * f64::from(native_rate)
                    / 16000.0
                    + domain.native_offset_frames;
                squared += (f64::from(sample) - signal(native)).powi(2);
            }
            let rms = (squared / (end - begin) as f64).sqrt();
            assert!(
                rms < 0.00002,
                "{native_rate}->{archive_rate}->16000 RMS {rms}; {domain:?}"
            );
        }
    }

    fn chain() -> ConversionChain {
        ConversionChain {
            native_rate_hz: 48000,
            archive_rate_hz: 48000,
            archive_phase: ConversionOrigin {
                input_frame_numerator: 0,
                denominator: 1,
            },
            canonical_phase: ConversionOrigin {
                input_frame_numerator: 0,
                denominator: 1,
            },
            archive_start_frame: 0,
            canonical_anchor: ProcessingFrame(0),
            native_start_frame: NativeFrame(0),
        }
    }

    #[test]
    fn large_recording_positions_do_not_round_away_the_generation_fraction() {
        let mut mapping = chain();
        mapping.archive_start_frame = (1u64 << 54) + 1;
        mapping.canonical_anchor = ProcessingFrame(2192);
        let actual = mapping.domain().unwrap();
        assert_eq!(
            actual.processing_origin.0,
            2192 + mapping.archive_start_frame / 3
        );
        assert_eq!(
            actual.native_offset_frames,
            -((mapping.archive_start_frame % 3) as f64)
        );
    }

    #[test]
    fn invalid_or_overflowing_conversion_geometry_is_refused() {
        let mut mapping = chain();
        mapping.archive_phase.denominator = 0;
        assert!(mapping.domain().is_err());
        mapping = chain();
        mapping.archive_rate_hz = 0;
        assert!(mapping.domain().is_err());
        mapping = chain();
        mapping.canonical_anchor = ProcessingFrame(u64::MAX);
        mapping.archive_start_frame = 48000;
        assert!(mapping.domain().is_err());
    }

    fn observation(sequence: u64, frame: u64) -> CaptureObservation {
        CaptureObservation {
            sequence,
            sample_start: frame * 2,
            sample_count: 960,
            device_frames: frame + 9000,
            host_ns: 1_000_000_000 + frame * 1_000_000_000 / 48000,
            flags: 0,
            dropped_samples: 0,
            timing_faults: 0,
        }
    }

    #[test]
    fn a_latest_callback_is_not_the_time_of_an_older_ring_drain() {
        let mut clock = SourceClock::new(48000, 2, 3).unwrap();
        let first = observation(1, 0);
        clock.observe(first, first.host_ns).unwrap();
        let latest = observation(2, 480);
        clock.observe(latest, latest.host_ns).unwrap();
        assert_eq!(
            clock.native_start(0, 960, latest.host_ns),
            Some(NativeFrame(0))
        );
        assert_eq!(
            clock.native_start(960, 960, latest.host_ns),
            Some(NativeFrame(480))
        );
        assert!(
            clock.native_start(1920, 2, latest.host_ns).is_none(),
            "future accepted samples have no evidence"
        );
        assert!(clock.native_start(1, 960, latest.host_ns).is_none());
        assert!(clock.native_start(0, 959, latest.host_ns).is_none());
        assert!(
            clock
                .native_start(0, 960, latest.host_ns + 1_000_000_001)
                .is_none()
        );
    }

    #[test]
    fn superseded_timing_faults_and_drops_fence_queued_pcm_until_a_new_range_is_known() {
        let mut clock = SourceClock::new(48000, 2, 1).unwrap();
        let first = observation(1, 0);
        clock.observe(first, first.host_ns).unwrap();
        assert!(clock.native_start(0, 960, first.host_ns).is_some());
        let mut fault = observation(3, 960);
        fault.timing_faults = 1; // the flagged intermediate callback was superseded
        assert!(clock.observe(fault, fault.host_ns).is_err());
        assert!(clock.native_start(0, 960, fault.host_ns).is_none());
        let mut recovered = observation(4, 1440);
        recovered.timing_faults = 1;
        clock.observe(recovered, recovered.host_ns).unwrap();
        assert!(clock.native_start(1920, 960, recovered.host_ns).is_none());
        assert_eq!(
            clock.native_start(2880, 960, recovered.host_ns),
            Some(NativeFrame(1440))
        );
        let mut dropped = observation(5, 1920);
        dropped.dropped_samples = 960;
        dropped.timing_faults = 1;
        assert!(clock.observe(dropped, dropped.host_ns).is_err());
        assert!(clock.native_start(2880, 960, dropped.host_ns).is_none());
    }
}

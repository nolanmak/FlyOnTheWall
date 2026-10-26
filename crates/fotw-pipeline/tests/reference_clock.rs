use fotw_pipeline::{
    reference_clock::{Rejection, Tracker},
    ring::CaptureObservation,
};

fn callback(i: u64) -> CaptureObservation {
    CaptureObservation {
        sequence: i + 1,
        sample_start: i * 160,
        sample_count: 160,
        device_frames: i * 160,
        host_ns: 1_000_000_000 + i * 10_000_000,
        flags: 0,
        dropped_samples: 0,
        timing_faults: 0,
    }
}
fn recoverable() -> Tracker {
    let mut t = Tracker::with_timestamp_recovery(16000, 1).unwrap();
    for i in 0..241 {
        let o = callback(i);
        t.observe(1, o, o.host_ns + 20_000_000).unwrap();
    }
    t
}
#[test]
fn an_isolated_timestamp_can_revalidate_calibration_but_never_map_old_frames() {
    let mut t = recoverable();
    let mut fault = callback(241);
    fault.flags = 4;
    fault.timing_faults = 1;
    fault.host_ns += 5_000_000;
    assert_eq!(
        t.observe(1, fault, fault.host_ns + 20_000_000).unwrap_err(),
        Rejection::Fault
    );
    assert_eq!(
        t.observe(1, fault, fault.host_ns + 20_000_000).unwrap_err(),
        Rejection::Fault
    );
    let mut good = callback(242);
    good.timing_faults = 1;
    let fit = t
        .observe(1, good, good.host_ns + 20_000_000)
        .unwrap()
        .expect("fresh consistent evidence should revalidate the calibrated rate");
    assert!(fit.host_at_frame(241 * 160).is_none());
    assert!(fit.host_at_frame(242 * 160).is_some());
    let mut other = Tracker::new(16000, 1).unwrap();
    let mut healthy = None;
    for i in 0..243 {
        let o = callback(i);
        healthy = other.observe(1, o, o.host_ns + 20_000_000).unwrap();
    }
    assert!(
        fit.map_frame_from(&healthy.unwrap(), 240 * 160, good.host_ns + 20_000_000)
            .is_none(),
        "render lookup must also fence pre-fault positions"
    );
    use fotw_pipeline::reference_resample::{
        AlignmentEpoch, Config, Error, FrameDomain, HostNs, NativeFrame, ProcessingFrame,
        ReferenceBuffer, Request, TapGeneration,
    };
    let mut history = ReferenceBuffer::new(Config {
        render_rate_hz: 16000,
        capacity_frames: 16000,
        mic: FrameDomain {
            native_origin: NativeFrame(0),
            native_offset_frames: 0.0,
            processing_origin: ProcessingFrame(0),
            native_rate_hz: 16000,
        },
        render_generation: TapGeneration(1),
        mic_generation: TapGeneration(1),
        epoch: AlignmentEpoch(2),
    })
    .unwrap();
    history
        .append(
            AlignmentEpoch(2),
            TapGeneration(1),
            NativeFrame(240 * 160),
            &vec![0.25; 800],
        )
        .unwrap();
    let request = |start| Request {
        epoch: AlignmentEpoch(2),
        start: ProcessingFrame(start),
        now: HostNs(good.host_ns + 20_000_000),
        render: fit,
        mic: healthy.unwrap(),
    };
    let mut out = [99.0];
    assert_eq!(
        history.fill(request(242 * 160), &mut out),
        Err(Error::Clock),
        "interpolation support must not reach into the fenced interval"
    );
    assert_eq!(out, [99.0]);
    history.fill(request(242 * 160 + 40), &mut out).unwrap();
    assert!((out[0] - 0.25).abs() < 1e-6);
}
#[test]
fn recovery_does_not_bridge_drops_discontinuities_missing_callbacks_or_a_new_clock_offset() {
    for case in 0..10 {
        let mut t = recoverable();
        let mut fault = callback(241);
        fault.flags = 4;
        fault.timing_faults = 1;
        if case == 0 {
            fault.flags = 2;
        }
        if case == 1 {
            fault.dropped_samples = 160;
        }
        if case == 5 {
            fault.flags = 6;
        }
        if case == 6 {
            fault.timing_faults = 2;
        }
        assert!(t.observe(1, fault, fault.host_ns + 20_000_000).is_err());
        let mut good = callback(242);
        good.timing_faults = 1;
        good.dropped_samples = fault.dropped_samples;
        if case == 7 {
            good.flags = 4;
            good.timing_faults = 2;
        }
        if case == 8 {
            good.host_ns += 110_000_000;
        }
        if case == 9 {
            good.dropped_samples += 160;
        }
        if case == 2 {
            good.sequence += 1;
        }
        if case == 3 {
            good.host_ns += 200_000;
        }
        let result = t.observe(
            if case == 4 { 2 } else { 1 },
            good,
            good.host_ns + 20_000_000,
        );
        assert!(
            result.is_err() || result.unwrap().is_none(),
            "unsafe recovery case {case}"
        );
    }
}

#[test]
fn default_tracker_and_stale_calibration_cannot_use_timestamp_recovery() {
    for opt_in in [false, true] {
        let mut t = if opt_in {
            recoverable()
        } else {
            let mut t = Tracker::new(16000, 1).unwrap();
            for i in 0..241 {
                let o = callback(i);
                t.observe(1, o, o.host_ns + 20_000_000).unwrap();
            }
            t
        };
        let mut fault = callback(241);
        fault.flags = 4;
        fault.timing_faults = 1;
        // The fault itself is fresh, but the previous calibration is expired.
        if opt_in {
            fault.host_ns += 1_100_000_000;
        }
        assert_eq!(
            t.observe(1, fault, fault.host_ns + 20_000_000).unwrap_err(),
            Rejection::Fault
        );
        let mut good = callback(242);
        good.timing_faults = 1;
        if opt_in {
            good.host_ns += 1_100_000_000;
        }
        assert!(
            t.observe(1, good, good.host_ns + 20_000_000)
                .unwrap()
                .is_none()
        );
    }
}

fn observation(i: u64, ppm: f64, channels: u16) -> CaptureObservation {
    observation_at_rate(i, 16000, ppm, channels)
}
fn observation_at_rate(i: u64, rate: u32, ppm: f64, channels: u16) -> CaptureObservation {
    let frames = i * u64::from(rate / 10);
    CaptureObservation {
        // Ten 10ms callbacks between these latest-value observations.
        sequence: i * 10 + 1,
        sample_start: frames * u64::from(channels),
        sample_count: (rate as usize / 100) * usize::from(channels),
        device_frames: frames,
        host_ns: (frames as f64 / (f64::from(rate) * (1.0 + ppm / 1e6)) * 1e9).round() as u64,
        flags: 0,
        dropped_samples: 0,
        timing_faults: 0,
    }
}
fn ready(ppm: f64, channels: u16) -> Tracker {
    let mut tracker = Tracker::new(16000, channels).unwrap();
    for i in 0..40 {
        let o = observation(i, ppm, channels);
        tracker.observe(1, o, o.host_ns + 20_000_000).unwrap();
    }
    tracker
}

#[test]
fn consecutive_callbacks_cannot_hide_unaccounted_pcm_between_timestamps() {
    let mut t = ready(0.0, 1);
    let mut o = observation(40, 0.0, 1);
    o.sequence = observation(39, 0.0, 1).sequence + 1;
    assert_eq!(
        t.observe(1, o, o.host_ns).unwrap_err(),
        Rejection::Positions
    );
}

#[test]
fn callback_extent_cannot_create_an_unbounded_projection_range() {
    let mut t = Tracker::new(16000, 1).unwrap();
    let mut o = observation(0, 0.0, 1);
    o.sample_count = 16001;
    assert_eq!(t.observe(1, o, 0).unwrap_err(), Rejection::Positions);
}
#[test]
fn fits_both_drift_directions_and_stereo_without_using_nominal_rate_as_measurement() {
    for ppm in [-120.0, 0.0, 80.0] {
        for channels in [1, 2] {
            let mut t = ready(ppm, channels);
            let o = observation(40, ppm, channels);
            let fit = t
                .observe(1, o, o.host_ns + 20_000_000)
                .unwrap()
                .expect("two seconds of valid native timing must yield a rate");
            assert!((fit.rate_hz - 16000.0 * (1.0 + ppm / 1e6)).abs() < 0.001);
        }
    }
}
#[test]
fn maps_between_independent_sample_clocks_without_absolute_float_rounding() {
    let mut a = ready(80.0, 1);
    let oa = observation(40, 80.0, 1);
    let a = a.observe(1, oa, oa.host_ns).unwrap().unwrap();
    let mut b = ready(0.0, 1);
    let ob = observation(40, 0.0, 1);
    let b = b.observe(1, ob, ob.host_ns).unwrap().unwrap();
    let p = a.map_frame_from(&b, 48_000, ob.host_ns).unwrap();
    assert!((p.frame as f64 + p.fraction - 48003.84).abs() < 0.01);
    assert!(a.map_frame_from(&b, 1_000_000, ob.host_ns).is_none());
    assert!(
        a.map_frame_from(&b, 48_000, ob.host_ns + 1_000_000_001)
            .is_none()
    );
    let base = (1_u64 << 54) + 3;
    let mut high = Tracker::new(16000, 1).unwrap();
    let mut last = None;
    for i in 0..40 {
        let mut o = observation(i, 0.0, 1);
        o.sample_start += base;
        o.device_frames += base;
        o.host_ns += 1_u64 << 60;
        last = high.observe(7, o, o.host_ns).unwrap();
    }
    let fit = last.unwrap();
    let p = fit
        .map_frame_from(&fit, base + 48_001, (1_u64 << 60) + 4_000_000_000)
        .unwrap();
    assert_eq!(p.frame, base + 48_001);
    assert!(p.fraction.abs() < 1e-6);
}
#[test]
fn repeated_snapshots_cannot_manufacture_a_window_or_keep_a_stale_clock_alive() {
    let mut t = Tracker::new(16000, 1).unwrap();
    let o = observation(0, 0.0, 1);
    for _ in 0..100 {
        assert!(t.observe(1, o, 0).unwrap().is_none());
    }
    assert_eq!(
        t.observe(1, o, 1_000_000_001).unwrap_err(),
        Rejection::Stale
    );
    let mut t = ready(0.0, 1);
    let o = observation(40, 0.0, 1);
    assert_eq!(
        t.observe(1, o, o.host_ns - 1).unwrap_err(),
        Rejection::Stale
    );
}
#[test]
fn superseded_faults_drops_and_generations_fence_old_fits() {
    for fault in [true, false] {
        let mut t = ready(0.0, 1);
        let mut o = observation(40, 0.0, 1);
        if fault {
            o.timing_faults = 1;
        } else {
            o.dropped_samples = 2;
        }
        assert_eq!(t.observe(1, o, o.host_ns).unwrap_err(), Rejection::Fault);
        let mut next = observation(41, 0.0, 1);
        next.timing_faults = o.timing_faults;
        next.dropped_samples = o.dropped_samples;
        assert!(t.observe(1, next, next.host_ns).unwrap().is_none());
    }
    let mut t = ready(0.0, 1);
    let o = observation(0, 0.0, 1);
    assert!(t.observe(2, o, 0).unwrap().is_none());
    let old = observation(40, 0.0, 1);
    assert_eq!(
        t.observe(1, old, old.host_ns).unwrap_err(),
        Rejection::Superseded
    );
}
#[test]
fn inconsistent_positions_timestamp_flags_and_implausible_rates_are_rejected() {
    for flag in [2, 4, 8] {
        let mut t = ready(0.0, 1);
        let mut o = observation(40, 0.0, 1);
        o.flags = flag;
        assert_eq!(t.observe(1, o, o.host_ns).unwrap_err(), Rejection::Fault);
    }
    let mut t = ready(0.0, 2);
    let mut o = observation(40, 0.0, 2);
    o.sample_start += 2;
    assert_eq!(
        t.observe(1, o, o.host_ns).unwrap_err(),
        Rejection::Positions
    );
    let mut t = Tracker::new(16000, 1).unwrap();
    let mut rejected = false;
    for i in 0..40 {
        let o = observation(i, 5000.0, 1);
        rejected |= matches!(t.observe(1, o, o.host_ns), Err(Rejection::Fit));
    }
    assert!(rejected);
}

#[test]
fn native_capture_rates_are_fitted_in_frames_not_interleaved_samples() {
    for rate in [44100, 48000] {
        for channels in [1, 2] {
            let mut tracker = Tracker::new(rate, channels).unwrap();
            let mut fit = None;
            for i in 0..40 {
                let o = observation_at_rate(i, rate, 80.0, channels);
                fit = tracker.observe(1, o, o.host_ns + 20_000_000).unwrap();
            }
            assert!((fit.unwrap().rate_hz - f64::from(rate) * 1.00008).abs() < 0.001);
        }
    }
}

#[test]
fn timestamp_noise_is_bounded_and_a_large_outlier_invalidates_the_fit() {
    let mut t = Tracker::new(16000, 1).unwrap();
    let mut fit = None;
    for i in 0..2000 {
        let mut o = observation(i, 80.0, 1);
        o.host_ns += if i % 2 == 0 { 10_000 } else { 0 };
        fit = t.observe(1, o, o.host_ns + 50_000_000).unwrap();
        assert!(t.retained_observations() <= 64);
    }
    assert!((fit.unwrap().rate_hz / 16000.0 - 1.0 - 80.0 / 1e6).abs() < 1.0 / 1e6);
    let mut o = observation(2000, 80.0, 1);
    o.host_ns += 5_000_000;
    assert_eq!(t.observe(1, o, o.host_ns).unwrap_err(), Rejection::Fit);
}

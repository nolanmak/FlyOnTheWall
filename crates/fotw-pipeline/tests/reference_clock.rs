use fotw_pipeline::{
    reference_clock::{Rejection, Tracker},
    ring::CaptureObservation,
};

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

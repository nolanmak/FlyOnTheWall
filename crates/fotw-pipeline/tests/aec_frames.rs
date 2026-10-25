//! Behavioral adaptation contracts use a delayed, error-injectable fake backend.
use fotw_pipeline::aec::{
    Adapter, Backend, BackendState, BypassReason, Config, Error, Output, OutputKind,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Audit {
    captures: usize,
    renders: usize,
    resets: usize,
    render_ready: bool,
}
struct Delayed {
    frame: usize,
    delay: usize,
    queued: VecDeque<f32>,
    fail_at: Option<usize>,
    audit: Arc<Mutex<Audit>>,
}
impl Backend for Delayed {
    type Error = ();
    fn frame_samples(&self) -> usize {
        self.frame
    }
    fn delay_samples(&self) -> usize {
        self.delay
    }
    fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
        assert_eq!(frame.len(), self.frame);
        let mut a = self.audit.lock().unwrap();
        assert!(!a.render_ready);
        a.render_ready = true;
        a.renders += 1;
        Ok(())
    }
    fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), ()> {
        assert_eq!(frame.len(), self.frame);
        assert_eq!(output.len(), self.frame);
        let mut a = self.audit.lock().unwrap();
        assert!(a.render_ready, "render must precede capture");
        a.render_ready = false;
        a.captures += 1;
        if self.fail_at == Some(a.captures) {
            return Err(());
        }
        for (sample, out) in frame.iter().zip(output) {
            self.queued.push_back(*sample * 0.5);
            *out = self.queued.pop_front().unwrap();
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<(), ()> {
        let mut a = self.audit.lock().unwrap();
        a.resets += 1;
        a.render_ready = false;
        self.queued = std::iter::repeat_n(0.0, self.delay).collect();
        Ok(())
    }
}
fn config() -> Config {
    Config {
        frame_samples: 160,
        delay_samples: 128,
        max_wait_samples: 320,
        max_reference_samples: 640,
    }
}
fn adapter(fail_at: Option<usize>) -> (Adapter<Delayed>, Arc<Mutex<Audit>>) {
    let audit = Arc::new(Mutex::new(Audit::default()));
    let backend = Delayed {
        frame: 160,
        delay: 128,
        queued: std::iter::repeat_n(0.0, 128).collect(),
        fail_at,
        audit: audit.clone(),
    };
    (Adapter::new(backend, config(), 7, 1000).unwrap(), audit)
}
fn mic(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.01 + (i % 97) as f32 * 0.003).collect()
}
fn flat(out: &[Output]) -> Vec<f32> {
    out.iter().flat_map(|b| b.samples.iter().copied()).collect()
}
fn check_clock(out: &[Output], epoch: u64, start: u64, n: usize) {
    let mut next = start;
    for b in out {
        assert_eq!(b.epoch, epoch);
        assert_eq!(b.start_sample, next);
        next += b.samples.len() as u64;
    }
    assert_eq!(next, start + n as u64);
}
#[test]
fn arbitrary_chunks_and_partial_stop_release_the_exact_delayed_source_tail() {
    let (mut a, audit) = adapter(None);
    let mut out = Vec::new();
    let near = mic(479);
    let render = vec![0.2; 479];
    let before = render.clone();
    a.push_render(7, 1000, &render, &mut out).unwrap();
    let mut at = 0;
    for n in [1, 158, 7, 300, 13] {
        a.push_capture(7, 1000 + at as u64, &near[at..at + n], &mut out)
            .unwrap();
        at += n;
    }
    a.finish(&mut out);
    assert_eq!(flat(&out), near.iter().map(|n| n * 0.5).collect::<Vec<_>>());
    check_clock(&out, 7, 1000, 479);
    assert_eq!(render, before);
    assert!(out.iter().all(|b| b.kind == OutputKind::Cleaned));
    assert_eq!(a.stats().cleaned_samples, 479);
    assert_eq!(a.buffered_samples(), 0);
    let audit = audit.lock().unwrap();
    assert!(audit.captures > 0);
    assert_eq!(audit.renders, audit.captures);
}
#[test]
fn reference_arriving_after_capture_within_the_budget_can_still_clean_it() {
    let (mut a, _) = adapter(None);
    let mut out = Vec::new();
    let near = mic(160);
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    assert!(out.is_empty());
    a.push_render(7, 1000, &[0.2; 160], &mut out).unwrap();
    a.finish(&mut out);
    assert_eq!(flat(&out), near.iter().map(|n| n * 0.5).collect::<Vec<_>>());
    check_clock(&out, 7, 1000, 160);
}
#[test]
fn missing_reference_passes_every_original_sample_with_bounded_retention() {
    let (mut a, audit) = adapter(None);
    let mut out = Vec::new();
    let near = mic(5000);
    let mut at = 0;
    for block in near.chunks(37) {
        a.push_capture(7, 1000 + at, block, &mut out).unwrap();
        at += block.len() as u64;
        assert!(a.buffered_samples() <= 128 + 320 + 160 + 640);
    }
    a.finish(&mut out);
    assert_eq!(flat(&out), near);
    check_clock(&out, 7, 1000, 5000);
    assert!(
        out.iter()
            .all(|b| b.kind == OutputKind::Raw(BypassReason::MissingReference))
    );
    let audit = audit.lock().unwrap();
    assert_eq!(audit.captures, 0);
    assert_eq!(audit.renders, 0);
    assert_eq!(
        audit.resets, 0,
        "missing references must not repeatedly reset an unused loaded backend"
    );
}

#[test]
fn reference_eviction_or_a_reference_gap_preserves_original_clock_and_marks_bypassed_intervals() {
    let (mut a, _) = adapter(None);
    let mut out = Vec::new();
    let near = mic(5000);
    a.push_render(7, 1000, &vec![0.2; 5000], &mut out).unwrap();
    assert!(a.buffered_samples() <= 640);
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    a.finish(&mut out);
    check_clock(&out, 7, 1000, 5000);
    assert!(out.iter().any(|b| matches!(b.kind, OutputKind::Raw(_))));
    assert!(out.iter().any(|b| b.kind == OutputKind::Cleaned));
    for block in out {
        let from = (block.start_sample - 1000) as usize;
        let expected: Vec<_> = near[from..from + block.samples.len()]
            .iter()
            .map(|n| {
                if block.kind == OutputKind::Cleaned {
                    n * 0.5
                } else {
                    *n
                }
            })
            .collect();
        assert_eq!(block.samples, expected);
    }
    let (mut a, _) = adapter(None);
    let mut out = Vec::new();
    let near = mic(320);
    a.push_capture(7, 1000, &near[..160], &mut out).unwrap();
    a.push_render(7, 1080, &[0.2; 160], &mut out).unwrap();
    a.push_capture(7, 1160, &near[160..], &mut out).unwrap();
    a.push_render(7, 1240, &[0.2; 80], &mut out).unwrap();
    a.finish(&mut out);
    let expected: Vec<_> = near
        .iter()
        .enumerate()
        .map(|(i, n)| if i < 160 { *n } else { n * 0.5 })
        .collect();
    assert_eq!(flat(&out), expected);
    check_clock(&out, 7, 1000, 320);
}

#[test]
fn nonfinite_backend_output_falls_back_to_original_audio_and_stays_fenced() {
    struct BadOutput(Delayed);
    impl Backend for BadOutput {
        type Error = ();
        fn frame_samples(&self) -> usize {
            self.0.frame
        }
        fn delay_samples(&self) -> usize {
            self.0.delay
        }
        fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
            self.0.analyze_render(frame)
        }
        fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), ()> {
            self.0.process_capture(frame, output)?;
            output[5] = f32::NAN;
            Ok(())
        }
        fn reset(&mut self) -> Result<(), ()> {
            self.0.reset()
        }
    }
    let audit = Arc::new(Mutex::new(Audit::default()));
    let backend = BadOutput(Delayed {
        frame: 160,
        delay: 128,
        queued: std::iter::repeat_n(0.0, 128).collect(),
        fail_at: None,
        audit: audit.clone(),
    });
    let mut a = Adapter::new(backend, config(), 7, 1000).unwrap();
    let mut out = Vec::new();
    let near = mic(480);
    a.push_render(7, 1000, &[0.2; 480], &mut out).unwrap();
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    a.finish(&mut out);
    assert_eq!(flat(&out), near);
    check_clock(&out, 7, 1000, 480);
    assert_eq!(audit.lock().unwrap().captures, 1);
    assert_eq!(a.stats().backend_failures, 1);
    assert!(
        out.iter()
            .all(|b| b.kind == OutputKind::Raw(BypassReason::BackendFailed))
    );
}
#[test]
fn backend_error_preserves_the_unpublished_delayed_originals_and_fences_later_calls() {
    let (mut a, audit) = adapter(Some(2));
    let mut out = Vec::new();
    let near = mic(480);
    a.push_render(7, 1000, &[0.2; 480], &mut out).unwrap();
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    a.finish(&mut out);
    let expected: Vec<_> = near
        .iter()
        .enumerate()
        .map(|(i, n)| if i < 32 { n * 0.5 } else { *n })
        .collect();
    assert_eq!(flat(&out), expected);
    check_clock(&out, 7, 1000, 480);
    assert_eq!(audit.lock().unwrap().captures, 2);
    assert_eq!(a.stats().backend_failures, 1);
    assert!(
        out.iter()
            .any(|b| b.kind == OutputKind::Raw(BypassReason::BackendFailed))
    );
    assert_eq!(a.backend_state(), BackendState::Failed);
    assert_eq!(a.reset(8, 3000, &mut out).unwrap(), BackendState::Available);
    let mut recovered = Vec::new();
    let near = mic(160);
    a.push_render(8, 3000, &[0.2; 160], &mut recovered).unwrap();
    a.push_capture(8, 3000, &near, &mut recovered).unwrap();
    a.finish(&mut recovered);
    assert_eq!(
        flat(&recovered),
        near.iter().map(|n| n * 0.5).collect::<Vec<_>>()
    );
    check_clock(&recovered, 8, 3000, 160);
    assert_eq!(audit.lock().unwrap().captures, 4);
    assert_eq!(a.stats().backend_failures, 1);
}

#[test]
fn failed_route_reset_retains_the_old_tail_and_new_audio_without_reentering_the_backend() {
    struct BadReset(Delayed);
    impl Backend for BadReset {
        type Error = ();
        fn frame_samples(&self) -> usize {
            self.0.frame
        }
        fn delay_samples(&self) -> usize {
            self.0.delay
        }
        fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
            self.0.analyze_render(frame)
        }
        fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), ()> {
            self.0.process_capture(frame, output)
        }
        fn reset(&mut self) -> Result<(), ()> {
            self.0.audit.lock().unwrap().resets += 1;
            Err(())
        }
    }
    let audit = Arc::new(Mutex::new(Audit::default()));
    let backend = BadReset(Delayed {
        frame: 160,
        delay: 128,
        queued: std::iter::repeat_n(0.0, 128).collect(),
        fail_at: None,
        audit: audit.clone(),
    });
    let mut a = Adapter::new(backend, config(), 7, 1000).unwrap();
    let mut out = Vec::new();
    let near = mic(160);
    a.push_render(7, 1000, &[0.2; 160], &mut out).unwrap();
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    assert_eq!(a.reset(8, 5000, &mut out).unwrap(), BackendState::Failed);
    check_clock(&out, 7, 1000, 160);
    let expected: Vec<_> = near
        .iter()
        .enumerate()
        .map(|(i, n)| if i < 32 { n * 0.5 } else { *n })
        .collect();
    assert_eq!(flat(&out), expected);
    let mut fresh = Vec::new();
    a.push_render(8, 5000, &[0.2; 160], &mut fresh).unwrap();
    a.push_capture(8, 5000, &near, &mut fresh).unwrap();
    a.finish(&mut fresh);
    assert_eq!(flat(&fresh), near);
    check_clock(&fresh, 8, 5000, 160);
    assert!(
        fresh
            .iter()
            .all(|b| b.kind == OutputKind::Raw(BypassReason::BackendFailed))
    );
    let audit = audit.lock().unwrap();
    assert_eq!(audit.captures, 1);
    assert_eq!(audit.resets, 1);
    assert_eq!(a.stats().backend_failures, 1);
}

#[test]
fn a_declared_shape_mismatch_is_rejected_before_native_frame_calls_can_panic() {
    let audit = Arc::new(Mutex::new(Audit::default()));
    let backend = Delayed {
        frame: 160,
        delay: 128,
        queued: std::iter::repeat_n(0.0, 128).collect(),
        fail_at: None,
        audit: audit.clone(),
    };
    let mut wrong = config();
    wrong.frame_samples = 256;
    assert!(matches!(
        Adapter::new(backend, wrong, 7, 1000),
        Err(Error::Config)
    ));
    assert_eq!(audit.lock().unwrap().captures, 0);
}

#[test]
fn route_reset_releases_delayed_originals_and_refuses_old_generation_references() {
    let (mut a, audit) = adapter(None);
    let mut out = Vec::new();
    let near = mic(160);
    a.push_render(7, 1000, &[0.2; 160], &mut out).unwrap();
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    assert_eq!(flat(&out).len(), 32);
    a.reset(8, 5000, &mut out).unwrap();
    let old: Vec<_> = near
        .iter()
        .enumerate()
        .map(|(i, n)| if i < 32 { n * 0.5 } else { *n })
        .collect();
    assert_eq!(flat(&out), old);
    check_clock(&out, 7, 1000, 160);
    assert_eq!(
        out.last().unwrap().kind,
        OutputKind::Raw(BypassReason::Reset)
    );
    assert_eq!(
        a.push_render(7, 5000, &[0.2; 160], &mut out),
        Err(Error::Epoch)
    );
    let mut fresh = Vec::new();
    a.push_capture(8, 5000, &near, &mut fresh).unwrap();
    a.finish(&mut fresh);
    assert_eq!(flat(&fresh), near);
    check_clock(&fresh, 8, 5000, 160);
    assert!(audit.lock().unwrap().resets >= 1);
}

#[test]
fn a_stalled_reference_deadline_and_repeated_stop_do_not_drop_or_duplicate_tail_audio() {
    let (mut a, _) = adapter(None);
    let mut out = Vec::new();
    let near = mic(79);
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    assert!(out.is_empty());
    a.flush_reference_wait(&mut out);
    assert_eq!(flat(&out), near);
    a.finish(&mut out);
    a.finish(&mut out);
    assert_eq!(flat(&out), near);
    assert_eq!(
        a.push_capture(7, 1079, &[0.1], &mut out),
        Err(Error::Closed)
    );
}

#[test]
fn supported_frame_shapes_and_multi_frame_delays_preserve_empty_aligned_and_partial_inputs() {
    for frame in [160, 256, 512] {
        for delay in [0, 1, 128, frame - 1, frame + 1] {
            for n in [0, 1, frame - 1, frame, frame + 1, frame * 2 + 17] {
                let audit = Arc::new(Mutex::new(Audit::default()));
                let backend = Delayed {
                    frame,
                    delay,
                    queued: std::iter::repeat_n(0.0, delay).collect(),
                    fail_at: None,
                    audit,
                };
                let cfg = Config {
                    frame_samples: frame,
                    delay_samples: delay,
                    max_wait_samples: frame * 2,
                    max_reference_samples: frame * 4,
                };
                let mut a = Adapter::new(backend, cfg, 3, 20000).unwrap();
                let mut out = Vec::new();
                let near = mic(n);
                a.push_render(3, 20000, &vec![0.2; n], &mut out).unwrap();
                let mut at = 0;
                for block in near.chunks(37) {
                    a.push_capture(3, 20000 + at, block, &mut out).unwrap();
                    at += block.len() as u64;
                }
                a.finish(&mut out);
                assert_eq!(
                    flat(&out),
                    near.iter().map(|n| n * 0.5).collect::<Vec<_>>(),
                    "frame={frame} delay={delay} n={n}"
                );
                check_clock(&out, 3, 20000, n);
                assert_eq!(a.stats().cleaned_samples, n as u64);
                assert_eq!(a.buffered_samples(), 0);
            }
        }
    }
}

#[test]
fn rejected_input_cannot_partially_advance_or_publish_the_microphone_clock() {
    let (mut a, audit) = adapter(None);
    let mut out = Vec::new();
    assert_eq!(a.push_capture(8, 1000, &[0.2], &mut out), Err(Error::Epoch));
    assert_eq!(
        a.push_capture(7, 999, &[0.2], &mut out),
        Err(Error::Discontinuous)
    );
    assert_eq!(
        a.push_capture(7, 1000, &[f32::NAN], &mut out),
        Err(Error::InvalidSamples)
    );
    assert_eq!(
        a.push_capture(7, u64::MAX, &[0.2], &mut out),
        Err(Error::Overflow)
    );
    assert!(out.is_empty());
    assert_eq!(a.buffered_samples(), 0);
    assert_eq!(audit.lock().unwrap().captures, 0);
    a.push_capture(7, 1000, &[0.3], &mut out).unwrap();
    a.finish(&mut out);
    assert_eq!(flat(&out), vec![0.3]);
    check_clock(&out, 7, 1000, 1);
}

#[test]
fn jittered_independent_delivery_preserves_sample_order_and_degradation_provenance() {
    let near: Vec<_> = (0..50017).map(|i| 0.01 + i as f32 / 1_000_000.0).collect();
    let render = vec![0.2; near.len()];
    for mut seed in [1u32, 17, 10003] {
        let (mut a, _) = adapter(None);
        let mut out = Vec::new();
        let (mut capture, mut reference) = (0, 0);
        while capture < near.len() || reference < render.len() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = (seed as usize % 701) + 1;
            if reference < render.len() && (capture == near.len() || seed & 1 == 0) {
                let end = (reference + n).min(render.len());
                a.push_render(
                    7,
                    1000 + reference as u64,
                    &render[reference..end],
                    &mut out,
                )
                .unwrap();
                reference = end;
            } else {
                let end = (capture + n).min(near.len());
                a.push_capture(7, 1000 + capture as u64, &near[capture..end], &mut out)
                    .unwrap();
                capture = end;
            }
            assert!(a.buffered_samples() <= 128 + 320 + 160 + 640);
        }
        a.finish(&mut out);
        check_clock(&out, 7, 1000, near.len());
        for block in &out {
            let from = (block.start_sample - 1000) as usize;
            for (i, actual) in block.samples.iter().enumerate() {
                let expected = near[from + i]
                    * if block.kind == OutputKind::Cleaned {
                        0.5
                    } else {
                        1.0
                    };
                assert_eq!(*actual, expected, "incorrect source sample {}", from + i);
            }
        }
        let stats = a.stats();
        assert_eq!(stats.cleaned_samples + stats.raw_samples, near.len() as u64);
        assert_eq!(stats.backend_failures, 0);
        assert_eq!(a.buffered_samples(), 0);
    }
}

#[test]
fn a_backend_that_reports_success_without_writing_a_full_frame_cannot_publish_silence() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct PartialOutput(Arc<AtomicUsize>);
    impl Backend for PartialOutput {
        type Error = ();
        fn frame_samples(&self) -> usize {
            160
        }
        fn delay_samples(&self) -> usize {
            128
        }
        fn analyze_render(&mut self, _: &[f32]) -> Result<(), ()> {
            Ok(())
        }
        fn process_capture(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            output[0] = input[0] * 0.5;
            Ok(())
        }
        fn reset(&mut self) -> Result<(), ()> {
            Ok(())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut a = Adapter::new(PartialOutput(calls.clone()), config(), 7, 1000).unwrap();
    let near = mic(160);
    let mut out = Vec::new();
    a.push_render(7, 1000, &[0.2; 160], &mut out).unwrap();
    a.push_capture(7, 1000, &near, &mut out).unwrap();
    a.finish(&mut out);
    assert_eq!(flat(&out), near);
    check_clock(&out, 7, 1000, 160);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        out.iter()
            .all(|b| b.kind == OutputKind::Raw(BypassReason::BackendFailed))
    );
}

#[test]
fn a_backend_shape_change_during_reset_is_fenced_before_the_next_native_frame() {
    struct ChangedShape(Delayed);
    impl Backend for ChangedShape {
        type Error = ();
        fn frame_samples(&self) -> usize {
            self.0.frame
        }
        fn delay_samples(&self) -> usize {
            self.0.delay
        }
        fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
            self.0.analyze_render(frame)
        }
        fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), ()> {
            self.0.process_capture(frame, output)
        }
        fn reset(&mut self) -> Result<(), ()> {
            self.0.frame = 256;
            self.0.reset()
        }
    }
    let audit = Arc::new(Mutex::new(Audit::default()));
    let backend = ChangedShape(Delayed {
        frame: 160,
        delay: 128,
        queued: std::iter::repeat_n(0.0, 128).collect(),
        fail_at: None,
        audit: audit.clone(),
    });
    let mut a = Adapter::new(backend, config(), 7, 1000).unwrap();
    let mut out = Vec::new();
    assert_eq!(a.reset(8, 5000, &mut out).unwrap(), BackendState::Failed);
    let near = mic(160);
    a.push_render(8, 5000, &[0.2; 160], &mut out).unwrap();
    a.push_capture(8, 5000, &near, &mut out).unwrap();
    a.finish(&mut out);
    assert_eq!(flat(&out), near);
    check_clock(&out, 8, 5000, 160);
    assert_eq!(audit.lock().unwrap().captures, 0);
}

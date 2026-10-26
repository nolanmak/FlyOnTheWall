use fotw_pipeline::{
    aec::{Backend, Config as AdapterConfig, Output, OutputKind},
    aec_worker::{Config, Worker},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Fake {
    entered: Option<mpsc::Sender<()>>,
    release: Option<mpsc::Receiver<()>>,
    dropped: Arc<AtomicBool>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}
impl Backend for Fake {
    type Error = ();
    fn frame_samples(&self) -> usize {
        160
    }
    fn delay_samples(&self) -> usize {
        0
    }
    fn analyze_render(&mut self, _: &[f32]) -> Result<(), ()> {
        Ok(())
    }
    fn process_capture(&mut self, frame: &[f32], out: &mut [f32]) -> Result<(), ()> {
        if let Some(tx) = self.entered.take() {
            tx.send(()).unwrap();
        }
        if let Some(rx) = self.release.take() {
            rx.recv().unwrap();
        }
        for (a, b) in out.iter_mut().zip(frame) {
            *a = *b * 0.5;
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<(), ()> {
        Ok(())
    }
}
fn config() -> Config {
    Config {
        adapter: AdapterConfig {
            frame_samples: 160,
            delay_samples: 0,
            max_wait_samples: 320,
            max_reference_samples: 640,
        },
        queued_blocks: 1,
        block_samples: 320,
        pending_samples: 640,
        deadline_ms: 100,
    }
}
fn blocked() -> (
    Worker,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
    Arc<AtomicBool>,
) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let w = Worker::new(
        Fake {
            entered: Some(entered_tx),
            release: Some(release_rx),
            dropped: dropped.clone(),
        },
        config(),
        1,
        0,
    )
    .unwrap();
    (w, entered_rx, release_tx, dropped)
}
fn until(mut test: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !test() {
        assert!(Instant::now() < deadline, "worker failed to make progress");
        std::thread::yield_now();
    }
}
fn pcm(out: &[Output]) -> Vec<f32> {
    out.iter().flat_map(|b| b.samples.iter().copied()).collect()
}

#[test]
fn deadline_releases_exact_originals_and_fences_late_native_output() {
    let (mut w, entered, release, _) = blocked();
    let mut out = Vec::new();
    w.submit(1, 0, &[0.8; 160], Some(&[0.2; 160]), 0, &mut out)
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("backend never received admitted audio");
    assert_eq!(w.pending_samples(), 160);
    w.poll(100, &mut out).unwrap();
    assert_eq!(pcm(&out), vec![0.8; 160]);
    assert!(matches!(out[0].kind, OutputKind::Raw(_)));
    assert!(w.status().is_running());
    w.reset(2, 1000, &mut out).unwrap();
    w.submit(2, 1000, &[0.6; 160], Some(&[0.2; 160]), 100, &mut out)
        .unwrap();
    release.send(()).unwrap();
    until(|| {
        w.poll(100, &mut out).unwrap();
        w.pending_samples() == 0
    });
    assert_eq!(out.len(), 2);
    assert_eq!((out[1].epoch, out[1].start_sample), (2, 1000));
    assert_eq!(out[1].samples, vec![0.3; 160]);
    assert_eq!(out[1].kind, OutputKind::Cleaned);
}

#[test]
fn stop_is_bounded_but_resource_status_waits_for_actual_backend_destruction() {
    let (mut w, entered, release, dropped) = blocked();
    let mut out = Vec::new();
    w.submit(1, 0, &[0.7; 160], Some(&[0.2; 160]), 0, &mut out)
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("backend never received admitted audio");
    let status = w.status();
    w.finish(1, &mut out).unwrap();
    w.poll(100, &mut out).unwrap();
    assert_eq!(pcm(&out), vec![0.7; 160]);
    assert!(status.is_running());
    assert!(!dropped.load(Ordering::Acquire));
    release.send(()).unwrap();
    until(|| !status.is_running());
    assert!(dropped.load(Ordering::Acquire));
    w.poll(101, &mut out).unwrap();
    assert_eq!(pcm(&out), vec![0.7; 160]);
    assert!(w.submit(1, 160, &[0.7; 160], None, 101, &mut out).is_err());
}

#[test]
fn full_queue_cannot_drop_originals_or_publish_stale_cleaned_results() {
    let (mut w, entered, release, _) = blocked();
    let mut out = Vec::new();
    w.submit(1, 0, &[0.8; 160], Some(&[0.2; 160]), 0, &mut out)
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("backend never received admitted audio");
    w.submit(1, 160, &[0.6; 160], Some(&[0.2; 160]), 1, &mut out)
        .unwrap();
    w.submit(1, 320, &[0.4; 160], Some(&[0.2; 160]), 2, &mut out)
        .unwrap();
    assert_eq!(
        pcm(&out),
        [vec![0.8; 160], vec![0.6; 160], vec![0.4; 160]].concat()
    );
    assert_eq!(w.pending_samples(), 0);
    assert!(out.iter().all(|o| matches!(o.kind, OutputKind::Raw(_))));
    w.finish(2, &mut out).unwrap();
    release.send(()).unwrap();
    until(|| !w.status().is_running());
    w.poll(3, &mut out).unwrap();
    assert_eq!(pcm(&out).len(), 480);
}

#[test]
fn arbitrary_chunks_and_partial_stop_preserve_every_source_sample() {
    let mut cfg = config();
    cfg.queued_blocks = 16;
    cfg.pending_samples = 16000;
    let mut w = Worker::new(
        Fake {
            entered: None,
            release: None,
            dropped: Arc::new(AtomicBool::new(false)),
        },
        cfg,
        7,
        9000,
    )
    .unwrap();
    let source: Vec<f32> = (0..1777).map(|n| (n % 71) as f32 / 100.0).collect();
    let mut out = Vec::new();
    let mut at = 0;
    for size in [37, 99, 129, 320, 21, 171, 320, 320, 320, 40] {
        let size = size.min(source.len() - at);
        w.submit(
            7,
            9000 + at as u64,
            &source[at..at + size],
            Some(&vec![0.1; size]),
            0,
            &mut out,
        )
        .unwrap();
        at += size;
    }
    assert_eq!(at, source.len());
    w.finish(0, &mut out).unwrap();
    until(|| {
        w.poll(0, &mut out).unwrap();
        !w.status().is_running() && w.pending_samples() == 0
    });
    let mut at = 0;
    for block in out {
        assert_eq!(block.epoch, 7);
        assert_eq!(block.start_sample, 9000 + at as u64);
        assert_eq!(block.kind, OutputKind::Cleaned);
        for sample in block.samples {
            assert_eq!(sample, source[at] * 0.5);
            at += 1;
        }
    }
    assert_eq!(at, source.len());
}

#[test]
fn invalid_calls_are_atomic_and_cancel_is_immediate_during_native_work() {
    use fotw_pipeline::aec_worker::Error;
    let (mut w, entered, release, _) = blocked();
    let mut out = Vec::new();
    for (epoch, start, mic, reference) in [
        (2, 0, vec![0.8; 160], None),
        (1, 1, vec![0.8; 160], None),
        (1, 0, vec![f32::NAN], None),
        (1, 0, vec![0.8; 321], None),
        (1, 0, vec![0.8; 160], Some(vec![0.2; 159])),
    ] {
        assert_eq!(
            w.submit(epoch, start, &mic, reference.as_deref(), 0, &mut out),
            Err(Error::Input)
        );
        assert_eq!(w.pending_samples(), 0);
        assert!(out.is_empty());
    }
    w.submit(1, 0, &[0.8; 160], Some(&[0.2; 160]), 10, &mut out)
        .unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(w.poll(9, &mut out), Err(Error::Clock));
    assert_eq!(w.pending_samples(), 160);
    assert!(out.is_empty());
    w.cancel(&mut out);
    assert_eq!(pcm(&out), vec![0.8; 160]);
    assert!(w.status().is_running());
    assert!(w.reset(2, 0, &mut out).is_err());
    release.send(()).unwrap();
    until(|| !w.status().is_running());
    w.poll(11, &mut out).unwrap();
    assert_eq!(pcm(&out), vec![0.8; 160]);
}

struct DestructorBlock {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for DestructorBlock {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
    }
}
impl Backend for DestructorBlock {
    type Error = ();
    fn frame_samples(&self) -> usize {
        160
    }
    fn delay_samples(&self) -> usize {
        0
    }
    fn analyze_render(&mut self, _: &[f32]) -> Result<(), ()> {
        Ok(())
    }
    fn process_capture(&mut self, _: &[f32], _: &mut [f32]) -> Result<(), ()> {
        unreachable!()
    }
    fn reset(&mut self) -> Result<(), ()> {
        Ok(())
    }
}
#[test]
fn status_does_not_release_a_native_resource_while_its_destructor_is_blocked() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut w = Worker::new(
        DestructorBlock {
            entered: entered_tx,
            release: release_rx,
        },
        config(),
        1,
        0,
    )
    .unwrap();
    let status = w.status();
    w.finish(0, &mut Vec::new()).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(status.is_running());
    drop(w);
    assert!(status.is_running());
    release_tx.send(()).unwrap();
    until(|| !status.is_running());
}

struct Panics(Arc<AtomicBool>);
impl Drop for Panics {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl Backend for Panics {
    type Error = ();
    fn frame_samples(&self) -> usize {
        160
    }
    fn delay_samples(&self) -> usize {
        0
    }
    fn analyze_render(&mut self, _: &[f32]) -> Result<(), ()> {
        panic!("synthetic native wrapper panic")
    }
    fn process_capture(&mut self, _: &[f32], _: &mut [f32]) -> Result<(), ()> {
        unreachable!()
    }
    fn reset(&mut self) -> Result<(), ()> {
        Ok(())
    }
}
#[test]
fn worker_panic_releases_originals_and_resource_before_status_becomes_idle() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut w = Worker::new(Panics(dropped.clone()), config(), 1, 0).unwrap();
    let mut out = Vec::new();
    w.submit(1, 0, &[0.7; 160], Some(&[0.2; 160]), 0, &mut out)
        .unwrap();
    until(|| !w.status().is_running());
    assert!(dropped.load(Ordering::Acquire));
    w.poll(1, &mut out).unwrap();
    assert_eq!(pcm(&out), vec![0.7; 160]);
    w.submit(1, 160, &[0.5; 160], None, 1, &mut out).unwrap();
    assert_eq!(pcm(&out), [vec![0.7; 160], vec![0.5; 160]].concat());
    assert_eq!(w.pending_samples(), 0);
    assert!(out.iter().all(|b| matches!(b.kind, OutputKind::Raw(_))));
}

#[test]
fn invalid_budgets_and_source_overflow_are_rejected_before_admission() {
    let fake = || Fake {
        entered: None,
        release: None,
        dropped: Arc::new(AtomicBool::new(false)),
    };
    for cfg in [
        Config {
            queued_blocks: 0,
            ..config()
        },
        Config {
            queued_blocks: 17,
            ..config()
        },
        Config {
            block_samples: 3201,
            ..config()
        },
        Config {
            pending_samples: 16001,
            ..config()
        },
        Config {
            pending_samples: 319,
            ..config()
        },
        Config {
            deadline_ms: 0,
            ..config()
        },
        Config {
            deadline_ms: 5001,
            ..config()
        },
    ] {
        assert!(Worker::new(fake(), cfg, 1, 0).is_err());
    }
    let mut w = Worker::new(fake(), config(), 1, u64::MAX - 10).unwrap();
    let mut out = Vec::new();
    assert!(
        w.submit(1, u64::MAX - 10, &[0.3; 11], None, 0, &mut out)
            .is_err()
    );
    assert!(out.is_empty());
    assert_eq!(w.pending_samples(), 0);
    w.cancel(&mut out);
}

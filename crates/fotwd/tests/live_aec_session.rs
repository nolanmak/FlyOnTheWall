//! Actual session/pump with generated, precisely clocked taps and a fake AEC.
//! No physical capture, model weights, credential store or external provider.
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapError, TapId};
use fotw_audio::{CaptureTimestamp, FrameFlags, FrameSink};
use fotw_pipeline::{
    aec::{Backend, Config},
    aec_worker::Config as WorkerConfig,
    replay::{ReplayReader, ReplaySource},
    resample::Downmixer,
};
use fotw_stt::{
    DeepgramEndpoint, DeepgramStreamConfig, Recognized, Recognizer, RecognizerFactory, Source,
    SttError,
};
use fotwd::{
    failover::{
        coverage::CoverageJournal,
        processing::{self, Descriptor},
        stream::FailoverConfig,
    },
    live_aec::Processing,
    session::{
        self, DeepgramLegs, FailoverLegs, LocalLegs, ReadySignal, SessionControl, Transcription,
    },
};
use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn far(frame: u64) -> f32 {
    (0.2 * (frame as f64 * std::f64::consts::TAU * 617.0 / 16000.0).sin()) as f32
}
fn pcm(index: u64, mic: bool) -> Vec<f32> {
    (0..160)
        .map(|n| far(index * 160 + n) + if mic { index as f32 * 0.0001 } else { 0.0 })
        .collect()
}
struct PulseTap {
    id: TapId,
    mic: bool,
    epoch: Arc<OnceLock<(Instant, u64)>>,
    ready: ReadySignal,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl PulseTap {
    fn pair(ready: ReadySignal) -> (Box<dyn AudioTap>, Box<dyn AudioTap>) {
        let epoch = Arc::new(OnceLock::new());
        let tap = |mic| {
            Box::new(Self {
                id: if mic {
                    TapId::mic("generated")
                } else {
                    TapId::system_default()
                },
                mic,
                epoch: epoch.clone(),
                ready: ready.clone(),
                stopped: Arc::new(AtomicBool::new(false)),
                thread: None,
            }) as Box<dyn AudioTap>
        };
        (tap(false), tap(true))
    }
}
impl AudioTap for PulseTap {
    fn id(&self) -> &TapId {
        &self.id
    }
    fn format(&self) -> StreamFormat {
        StreamFormat::new(16000, 1, SampleFormat::F32)
    }
    fn format_is_authoritative(&self) -> bool {
        true
    }
    fn start(&mut self, mut sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        let shared_epoch = self.epoch.clone();
        let ready = self.ready.clone();
        let stopped = self.stopped.clone();
        let mic = self.mic;
        self.thread = Some(std::thread::spawn(move || {
            // This fixture measures steady recording, not filesystem startup.
            // Reserve its full clock-qualification window after actual readiness.
            while !ready.is_ready() && !stopped.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            if stopped.load(Ordering::Acquire) {
                return;
            }
            let epoch =
                *shared_epoch.get_or_init(|| (Instant::now(), fotw_audio::clock::host_ns()));
            for i in 0..400 {
                let deadline = epoch.0 + Duration::from_millis(20 + i * 10);
                if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                // Fixture-native sample time, independent of callback scheduling.
                sink.on_frames(
                    &pcm(i, mic),
                    CaptureTimestamp::new(i * 160, epoch.1 + i * 10_000_000),
                    FrameFlags::empty(),
                );
            }
        }));
        Ok(self.format())
    }
    fn stop(&mut self) -> Result<(), TapError> {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        Ok(())
    }
}
impl Drop for PulseTap {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Model;
struct Factory;
impl RecognizerFactory for Factory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        None
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(Model))
    }
}
impl Recognizer for Model {
    fn transcribe(&mut self, _: &[f32]) -> Result<Vec<Recognized>, SttError> {
        Ok(Vec::new())
    }
}
struct Subtract {
    render: Vec<f32>,
    ready: ReadySignal,
    root: PathBuf,
    calls: Arc<AtomicUsize>,
}
impl Backend for Subtract {
    type Error = ();
    fn frame_samples(&self) -> usize {
        160
    }
    fn delay_samples(&self) -> usize {
        0
    }
    fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
        self.render = frame.to_vec();
        Ok(())
    }
    fn process_capture(&mut self, frame: &[f32], out: &mut [f32]) -> Result<(), ()> {
        assert_eq!(self.render.len(), 160, "render before capture");
        for ((o, m), r) in out.iter_mut().zip(frame).zip(&self.render) {
            *o = m - r;
        }
        let mean = out.iter().sum::<f32>() / 160.0;
        let index = (mean / 0.0001).round().clamp(0.0, 399.0) as u64;
        let session = self
            .ready
            .session_id()
            .expect("capture identity precedes processing");
        let directory = self.root.join(session);
        let bytes = (index + 1) * 160 * 2;
        assert!(
            std::fs::metadata(directory.join("mic.pcm")).unwrap().len() >= bytes,
            "original microphone frame must already be on disk"
        );
        assert!(
            std::fs::metadata(directory.join("system.pcm"))
                .unwrap()
                .len()
                >= bytes,
            "original reference must already be on disk"
        );
        self.render.clear();
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn reset(&mut self) -> Result<(), ()> {
        self.render.clear();
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Local,
    Cloud,
    Fallback,
}
async fn recording(mode: Mode) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("sessions");
    let mut control = SessionControl::new();
    let calls = Arc::new(AtomicUsize::new(0));
    control.processing = Some(
        Processing::new(
            Subtract {
                render: Vec::new(),
                ready: control.ready.clone(),
                root: root.clone(),
                calls: calls.clone(),
            },
            WorkerConfig {
                adapter: Config {
                    frame_samples: 160,
                    delay_samples: 0,
                    max_wait_samples: 320,
                    max_reference_samples: 640,
                },
                queued_blocks: 4,
                block_samples: 160,
                pending_samples: 16000,
                deadline_ms: 500,
            },
            Descriptor {
                backend: "fixture-subtractor".into(),
                version: "1".into(),
                profile: "generated-clocked-overlap".into(),
            },
        )
        .unwrap(),
    );
    let mut server = None;
    let transcription = if matches!(mode, Mode::Local) {
        Transcription::Local(LocalLegs::for_session(
            Arc::new(Factory),
            "placeholder",
            true,
            true,
        ))
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        server = Some(tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 8192);
                }
                stream
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
        }));
        let config = |source| {
            DeepgramStreamConfig::new(
                "fixture-not-a-key",
                fotw_stt::deepgram::DeepgramConfig::new("placeholder", source),
            )
            .with_endpoint(DeepgramEndpoint::loopback(port))
        };
        if matches!(mode, Mode::Cloud) {
            Transcription::Deepgram(DeepgramLegs {
                system: Box::new(config(Source::System)),
                mic: Some(Box::new(config(Source::Mic))),
            })
        } else {
            Transcription::Failover(Box::new(
                FailoverLegs::prepare(
                    FailoverConfig::new(config(Source::System), Arc::new(Factory)),
                    Some(FailoverConfig::new(config(Source::Mic), Arc::new(Factory))),
                )
                .await
                .unwrap(),
            ))
        }
    };
    let (system, mic) = PulseTap::pair(control.ready.clone());
    let stop = control.stop.clone();
    let ready = control.ready.clone();
    let stopper = tokio::spawn(async move {
        while !ready.is_ready() && !stop.is_stopped() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if ready.is_ready() {
            tokio::time::sleep(Duration::from_millis(4250)).await;
            stop.stop();
        }
    });
    let outcome = session::run_with_control(
        &root,
        system,
        Some(mic),
        transcription,
        Duration::from_secs(20),
        control,
    )
    .await
    .unwrap();
    stopper.await.unwrap();
    let id = outcome.dir.file_name().unwrap().to_string_lossy();
    let mut replay = ReplayReader::open(&outcome.dir, &id, ReplaySource::Mic).unwrap();
    let evidence = processing::validate(&outcome.dir, &id, ReplaySource::Mic, &mut replay)
        .unwrap()
        .expect("actual recording must declare its selected processor");
    assert!(
        evidence.cleaned_samples > 8000,
        "clock-qualified audio must reach AEC: evidence={evidence:?}, calls={}, errors={:?}",
        calls.load(Ordering::Acquire),
        outcome.stt_errors
    );
    assert!(calls.load(Ordering::Acquire) > 50);
    assert_eq!(evidence.end_sample - evidence.first_sample, 64000);
    assert_eq!(replay.saved_end().unwrap(), evidence.end_sample);
    for (name, mic) in [("system.pcm", false), ("mic.pcm", true)] {
        let expected: Vec<_> = (0..400)
            .flat_map(|i| Downmixer::to_i16(&pcm(i, mic)))
            .flat_map(i16::to_le_bytes)
            .collect();
        assert_eq!(
            std::fs::read(outcome.dir.join(name)).unwrap(),
            expected,
            "AEC must not change original capture"
        );
    }
    let coverage = CoverageJournal::snapshot(&outcome.dir, &id, Source::Mic).unwrap();
    assert_eq!(coverage.fallback.is_none(), matches!(mode, Mode::Cloud));
    if let Some(mut task) = server {
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                task.abort();
                panic!("expected both loopback-only sources");
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn local_session_archives_originals_and_persists_processed_input() {
    recording(Mode::Local).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn cloud_session_archives_originals_and_persists_processed_input() {
    recording(Mode::Cloud).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn fallback_session_archives_originals_and_persists_processed_input() {
    recording(Mode::Fallback).await;
}

struct DropGate {
    entered: Arc<AtomicBool>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}
impl Backend for DropGate {
    type Error = ();
    fn frame_samples(&self) -> usize {
        160
    }
    fn delay_samples(&self) -> usize {
        0
    }
    fn analyze_render(&mut self, _: &[f32]) -> Result<(), ()> {
        panic!("cancelled admission must not analyze render")
    }
    fn process_capture(&mut self, _: &[f32], _: &mut [f32]) -> Result<(), ()> {
        panic!("cancelled admission must not process capture")
    }
    fn reset(&mut self) -> Result<(), ()> {
        Ok(())
    }
}
impl Drop for DropGate {
    fn drop(&mut self) {
        self.entered.store(true, Ordering::Release);
        let mut ready = self.release.0.lock().unwrap();
        while !*ready {
            ready = self.release.1.wait(ready).unwrap();
        }
    }
}
struct ReleaseOnExit(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
impl Drop for ReleaseOnExit {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_processing_admission_retains_native_cleanup_and_cannot_be_reused() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let _release = ReleaseOnExit(release.clone());
    let entered = Arc::new(AtomicBool::new(false));
    let processing = Processing::new(
        DropGate {
            entered: entered.clone(),
            release: release.clone(),
        },
        WorkerConfig {
            adapter: Config {
                frame_samples: 160,
                delay_samples: 0,
                max_wait_samples: 320,
                max_reference_samples: 640,
            },
            queued_blocks: 4,
            block_samples: 160,
            pending_samples: 16000,
            deadline_ms: 50,
        },
        Descriptor {
            backend: "fixture-blocked-drop".into(),
            version: "1".into(),
            profile: "no-capture".into(),
        },
    )
    .unwrap();
    let mut control = SessionControl::new();
    control.processing = Some(processing.clone());
    control.stop.stop();
    let retained = control.clone();
    let (system, mic) = PulseTap::pair(control.ready.clone());
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        session::run_with_control(
            &sessions,
            system,
            Some(mic),
            Transcription::Disabled,
            Duration::from_secs(1),
            control,
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.contains("cancelled"));
    assert!(!sessions.exists());
    let until = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < until);
        tokio::task::yield_now().await;
    }
    assert!(processing.status().is_running());
    assert!(
        retained.speech.has_retained_native_worker(),
        "native cleanup remains part of recorder ownership"
    );
    let (system, mic) = PulseTap::pair(retained.ready.clone());
    let error = session::run_with_control(
        &sessions,
        system,
        Some(mic),
        Transcription::Disabled,
        Duration::from_secs(1),
        retained.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("already admitted"));
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    while processing.status().is_running() {
        assert!(Instant::now() < until);
        tokio::task::yield_now().await;
    }
    assert!(!retained.speech.has_retained_native_worker());
}

//! Synthetic platform contract: frozen scope reaches startup/rebuilds, and
//! excluded PCM cannot enter the real WAL/local-STT pipeline. This is not an
//! assertion about macOS process routing; native playback QA remains separate.
use fotw_audio::{
    AppInfo, AppRef, AudioPlatform, AudioTap, BoxFuture, CaptureTimestamp, DeviceId, DeviceInfo,
    FormatRequest, FrameFlags, FrameSink, Permission, PermissionState, PlatformCaps, PlatformEvent,
    SampleFormat, StreamFormat, SystemScope, TapError, TapId,
};
use fotw_audio::{
    DeviceWatch,
    device_change::{DeviceChangeKind, DeviceChangeSignal},
    testing::{ManualClock, MockPlatform},
};
use fotw_stt::{
    SttError,
    local::{Recognized, Recognizer, RecognizerFactory},
};
use fotwd::session::{self, LocalLegs, SessionControl, Transcription};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
type Sink = Arc<Mutex<Option<Box<dyn FrameSink>>>>;
struct Tap {
    id: TapId,
    sink: Sink,
    channels: u16,
}
impl AudioTap for Tap {
    fn id(&self) -> &TapId {
        &self.id
    }
    fn format(&self) -> StreamFormat {
        StreamFormat::new(16000, self.channels, SampleFormat::F32)
    }
    fn format_is_authoritative(&self) -> bool {
        true
    }
    fn start(&mut self, sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(self.format())
    }
    fn stop(&mut self) -> Result<(), TapError> {
        self.sink.lock().unwrap().take();
        Ok(())
    }
}
struct Platform {
    base: MockPlatform,
    system: Sink,
    mic: Sink,
    scopes: Mutex<Vec<SystemScope>>,
    signals: Mutex<Vec<Arc<DeviceChangeSignal>>>,
    included: AtomicU8,
    fail_reopen: bool,
}
impl Platform {
    fn event(&self, kind: DeviceChangeKind) {
        for s in self.signals.lock().unwrap().iter() {
            s.raise(kind);
        }
    }
}
impl AudioPlatform for Platform {
    fn caps(&self) -> PlatformCaps {
        self.base.caps()
    }
    fn permission(&self, p: Permission) -> PermissionState {
        self.base.permission(p)
    }
    fn request_permission(&self, p: Permission) -> BoxFuture<'static, PermissionState> {
        self.base.request_permission(p)
    }
    fn mics(&self) -> Vec<DeviceInfo> {
        self.base.mics()
    }
    fn capturable_apps(&self) -> Vec<AppInfo> {
        self.base.capturable_apps()
    }
    fn events(&self) -> std::sync::mpsc::Receiver<PlatformEvent> {
        self.base.events()
    }
    fn watch_devices(
        &self,
        signal: Arc<DeviceChangeSignal>,
    ) -> Result<Box<dyn DeviceWatch>, TapError> {
        self.signals.lock().unwrap().push(signal);
        Ok(Box::new(()))
    }
    fn open_system(
        &self,
        scope: SystemScope,
        _: FormatRequest,
    ) -> Result<Box<dyn AudioTap>, TapError> {
        let mut scopes = self.scopes.lock().unwrap();
        scopes.push(scope.clone());
        if self.fail_reopen && scopes.len() > 1 {
            return Err(TapError::unsupported(
                "fixture exclusion enforcement unavailable",
            ));
        }
        let mut included = 3;
        match scope {
            SystemScope::DefaultOutputMix => {}
            SystemScope::AllExcept(ids) => {
                if ids.contains(&AppRef::BundleId("com.spotify.client".into())) {
                    included &= !1;
                }
                if ids.contains(&AppRef::BundleId("com.brave.Browser.helper".into())) {
                    included &= !2;
                }
            }
            _ => return Err(TapError::unsupported("unexpected fixture scope")),
        }
        self.included.store(included, Ordering::SeqCst);
        Ok(Box::new(Tap {
            id: TapId::system_default(),
            sink: self.system.clone(),
            channels: 2,
        }))
    }
    fn open_mic(&self, _: &DeviceId, _: FormatRequest) -> Result<Box<dyn AudioTap>, TapError> {
        Ok(Box::new(Tap {
            id: TapId::mic("fixture"),
            sink: self.mic.clone(),
            channels: 1,
        }))
    }
}
#[derive(Clone)]
struct Inspect(Arc<Mutex<Vec<Vec<f32>>>>);
impl RecognizerFactory for Inspect {
    fn model(&self) -> &str {
        "synthetic-pcm-inspector"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(self.clone()))
    }
}
impl Recognizer for Inspect {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.0.lock().unwrap().push(pcm.to_vec());
        Ok(vec![])
    }
}
fn tone(sample: usize, hz: f32) -> f32 {
    (std::f32::consts::TAU * hz * sample as f32 / 16000.0).sin() * 0.1
}
fn magnitude(pcm: &[f32], hz: f32) -> f32 {
    // Maximum short-window projection survives archive gaps and ASR chunk boundaries.
    pcm.chunks_exact(1600)
        .map(|chunk| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in chunk.iter().enumerate() {
                let phase = std::f32::consts::TAU * hz * i as f32 / 16000.0;
                re += v * phase.cos();
                im += v * phase.sin();
            }
            (re * re + im * im).sqrt() * 2.0 / 1600.0
        })
        .fold(0.0, f32::max)
}
fn raw(path: &std::path::Path, channels: usize) -> Vec<f32> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(2 * channels)
        .map(|frame| f32::from(i16::from_le_bytes([frame[0], frame[1]])) / 32768.0)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Local,
    Cloud,
    Fallback,
}
struct Provider {
    port: u16,
    uploads: Arc<Mutex<Vec<Vec<f32>>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn provider(outage: bool) -> Provider {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{
        Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let uploads: Arc<Mutex<Vec<Vec<f32>>>> = Arc::default();
    let observed = uploads.clone();
    let task = tokio::spawn(async move {
        let mut clients = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let observed = observed.clone();
            clients.spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let index = {
                    let mut all = observed.lock().unwrap();
                    all.push(vec![]);
                    all.len() - 1
                };
                let mut received = 0;
                while let Some(Ok(message)) = socket.next().await {
                    match message {
                        Message::Binary(bytes) => {
                            let samples: Vec<_> = bytes
                                .chunks_exact(2)
                                .map(|s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0)
                                .collect();
                            received += samples.len();
                            observed.lock().unwrap()[index].extend(samples);
                            if outage && received >= 6400 {
                                socket
                                    .send(Message::Close(Some(CloseFrame {
                                        code: CloseCode::Restart,
                                        reason: "synthetic network interruption".into(),
                                    })))
                                    .await
                                    .unwrap();
                                break;
                            }
                        }
                        Message::Text(text) if text.contains("CloseStream") => {
                            let _ = socket.close(None).await;
                            break;
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            });
        }
        drop(listener);
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
    });
    Provider {
        port,
        uploads,
        task,
    }
}
struct EndCapture(session::StopSignal);
impl Drop for EndCapture {
    fn drop(&mut self) {
        self.0.stop();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn frozen_exclusions_survive_rebuilds_and_all_speech_paths() {
    for mode in [Mode::Local, Mode::Cloud, Mode::Fallback] {
        for (spotify, brave, fail_reopen) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (true, true, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let clock = ManualClock::new();
            let platform = Arc::new(Platform {
                base: MockPlatform::macos_taps(),
                system: Arc::default(),
                mic: Arc::default(),
                scopes: Mutex::new(vec![]),
                signals: Mutex::new(vec![]),
                included: AtomicU8::new(0),
                fail_reopen,
            });
            let control = SessionControl {
                system_scope: fotwd::capture_settings::scope(
                    fotw_web::capture::CaptureSettingsDoc {
                        ignore_spotify: spotify,
                        ignore_brave: brave,
                    },
                    true,
                    |id| {
                        Ok(vec![AppRef::BundleId(if id == "com.brave.Browser" {
                            "com.brave.Browser.helper".into()
                        } else {
                            id.into()
                        })])
                    },
                )
                .unwrap(),
                ..SessionControl::new()
            };
            let _end_capture = EndCapture(control.stop.clone());
            let expected = control.system_scope.clone();
            let inspected = Inspect(Arc::default());
            let mut legs = LocalLegs::for_session(
                Arc::new(inspected.clone()),
                "synthetic-exclusions",
                true,
                true,
            );
            for cfg in std::iter::once(&mut *legs.system).chain(legs.mic.as_deref_mut()) {
                cfg.endpoint.margin_db = 0.0;
                cfg.endpoint.speech_floor_db = -80.0;
                cfg.endpoint.min_range_db = 0.0;
                cfg.endpoint.min_speech_ms = 0;
                cfg.endpoint.hard_max_ms = 200;
            }
            let mut provider = if mode == Mode::Local {
                None
            } else {
                Some(provider(mode == Mode::Fallback).await)
            };
            let transcription = match mode {
                Mode::Local => Transcription::Local(legs),
                Mode::Cloud | Mode::Fallback => {
                    let port = provider.as_ref().unwrap().port;
                    let cloud = |source| {
                        fotw_stt::DeepgramStreamConfig::new(
                            "fixture-not-a-key",
                            fotw_stt::deepgram::DeepgramConfig::new("synthetic-exclusions", source),
                        )
                        .with_endpoint(fotw_stt::DeepgramEndpoint::loopback(port))
                    };
                    if mode == Mode::Cloud {
                        Transcription::Deepgram(session::DeepgramLegs {
                            system: Box::new(cloud(fotw_stt::Source::System)),
                            mic: Some(Box::new(cloud(fotw_stt::Source::Mic))),
                        })
                    } else {
                        let config = |source| {
                            let mut cfg = fotwd::failover::stream::FailoverConfig::new(
                                cloud(source),
                                Arc::new(inspected.clone()),
                            );
                            cfg.endpoint = legs.system.endpoint;
                            cfg.cloud_settle = Duration::from_millis(10);
                            cfg.stop_timeout = Duration::from_secs(3);
                            cfg.limits.retry_budget_ms = 15_000;
                            cfg.limits.failed_cycles = 1;
                            cfg
                        };
                        Transcription::Failover(Box::new(
                            session::FailoverLegs::prepare(
                                config(fotw_stt::Source::System),
                                Some(config(fotw_stt::Source::Mic)),
                            )
                            .await
                            .unwrap(),
                        ))
                    }
                }
            };
            let driver = async {
                tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        if control.ready.is_ready()
                            && platform.system.lock().unwrap().is_some()
                            && platform.mic.lock().unwrap().is_some()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("synthetic taps must start");
                // Supervision polls on wall time but debounces on ManualClock.
                // Keep delivering/advancing until a reopen is observed; freezing
                // the clock after 240 guessed scheduler turns can leave a late
                // event permanently inside the debounce window on a busy runner.
                let delivery_deadline = tokio::time::Instant::now() + Duration::from_secs(6);
                for frame in 0.. {
                    if frame >= 240 && platform.scopes.lock().unwrap().len() >= 2 {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < delivery_deadline,
                        "supervised reopen must occur while delivering frames: {mode:?}, Spotify={spotify}, Brave={brave}, fail_reopen={fail_reopen}"
                    );
                    clock.advance(Duration::from_millis(20));
                    // The refusal case deliberately delivers changes at the old
                    // final frame, so the late-event regression stays covered.
                    if frame == if fail_reopen { 239 } else { 70 } {
                        platform.event(DeviceChangeKind::DefaultOutput);
                    }
                    if frame == if fail_reopen { 239 } else { 130 } {
                        platform.event(DeviceChangeKind::WillSleep);
                    }
                    if frame == if fail_reopen { 239 } else { 150 } {
                        platform.event(DeviceChangeKind::DidWake);
                    }
                    let mask = platform.included.load(Ordering::SeqCst);
                    let mut system = Vec::with_capacity(640);
                    let mut mic = Vec::with_capacity(320);
                    for n in frame * 320..(frame + 1) * 320 {
                        let sample = tone(n, 400.0)
                            + if mask & 1 != 0 { tone(n, 800.0) } else { 0.0 }
                            + if mask & 2 != 0 { tone(n, 1200.0) } else { 0.0 };
                        system.extend([sample, sample]);
                        mic.push(tone(n, 1600.0));
                    }
                    if let Some(sink) = platform.system.lock().unwrap().as_mut() {
                        sink.on_frames(
                            &system,
                            CaptureTimestamp::new((frame * 320) as u64, clock.now_ns()),
                            FrameFlags::empty(),
                        );
                    }
                    if let Some(sink) = platform.mic.lock().unwrap().as_mut() {
                        sink.on_frames(
                            &mic,
                            CaptureTimestamp::new((frame * 320) as u64, clock.now_ns()),
                            FrameFlags::empty(),
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(4)).await;
                }
                if mode == Mode::Fallback {
                    // Prove the injected network loss demotes both sources
                    // while recording, rather than merely recovering a tail on Stop.
                    tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let sources = control.speech.snapshot();
                            if sources.len() == 2
                                && sources.iter().all(|source| {
                                    source.provider == fotw_stt::local::PROVIDER
                                        && source.switch_reason.as_deref()
                                            == Some("transport_budget")
                                })
                            {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .expect(
                        "both sources must hand off after the actual synthetic outage before Stop",
                    );
                }
                control.stop.stop();
            };
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(12), async {
                tokio::join!(
                    session::run_supervised(
                        root.path(),
                        platform.clone(),
                        clock.clone(),
                        transcription,
                        Duration::from_secs(60),
                        control.clone()
                    ),
                    driver
                )
            })
            .await
            .expect("synthetic recording must finish");
            let outcome = outcome.unwrap();
            let scopes = platform.scopes.lock().unwrap().clone();
            assert!(scopes.len() >= 2, "must exercise a real supervised reopen");
            assert!(
                scopes.iter().all(|scope| scope == &expected),
                "a rebuild widened the frozen scope"
            );
            let system = raw(&outcome.dir.join("system.pcm"), 2);
            let mic = raw(&outcome.dir.join("mic.pcm"), 1);
            assert!(magnitude(&system, 400.0) > 0.08, "meeting disappeared");
            assert!(magnitude(&mic, 1600.0) > 0.08, "microphone disappeared");
            for (hz, excluded) in [(800.0, spotify), (1200.0, brave)] {
                let level = magnitude(&system, hz);
                if excluded {
                    assert!(
                        level < 0.002,
                        "excluded system signal reached WAL: {hz}Hz={level}"
                    );
                } else {
                    assert!(level > 0.08, "unselected music app should remain included");
                }
            }
            // Archive the actual WAL, then inspect decoded content rather than
            // accepting a nonempty compressed file as proof of source exclusion.
            let encoded = fotw_pipeline::wal::encode_session(&outcome.dir).unwrap();
            for (track, is_mic) in [(&encoded.system, false), (&encoded.mic, true)] {
                let decoded =
                    fotw_pipeline::opus::decode_ogg_opus_file(outcome.dir.join(&track.file), 16000)
                        .unwrap();
                assert!(!decoded.truncated);
                let mono: Vec<_> = decoded
                    .samples
                    .iter()
                    .step_by(usize::from(decoded.channels))
                    .copied()
                    .collect();
                assert!(
                    magnitude(&mono, if is_mic { 1600.0 } else { 400.0 }) > 0.05,
                    "eligible source disappeared from compressed audio"
                );
                if !is_mic {
                    for (hz, excluded) in [(800.0, spotify), (1200.0, brave)] {
                        let level = magnitude(&mono, hz);
                        if excluded {
                            assert!(
                                level < 0.0125,
                                "excluded source reappeared in decoded audio: {hz}Hz={level}"
                            );
                        } else {
                            assert!(
                                level > 0.05,
                                "eligible source disappeared from decoded audio: {hz}Hz={level}"
                            );
                        }
                    }
                }
            }
            if mode != Mode::Cloud {
                let session_id = outcome.dir.file_name().unwrap().to_str().unwrap();
                for (source, is_mic) in [
                    (fotw_pipeline::replay::ReplaySource::System, false),
                    (fotw_pipeline::replay::ReplaySource::Mic, true),
                ] {
                    let mut reader =
                        fotw_pipeline::replay::ReplayReader::open(&outcome.dir, session_id, source)
                            .unwrap();
                    assert!(reader.is_complete().unwrap());
                    let mut cursor = reader.first_sample();
                    let mut samples = Vec::new();
                    loop {
                        let chunk = reader.read_from(cursor, 16000).unwrap();
                        if chunk.is_empty() {
                            break;
                        }
                        cursor += chunk.len() as u64;
                        samples.extend(chunk.into_iter().map(|s| f32::from(s) / 32768.0));
                    }
                    assert!(
                        magnitude(&samples, if is_mic { 1600.0 } else { 400.0 }) > 0.08,
                        "eligible source disappeared from durable replay"
                    );
                    if !is_mic {
                        for (hz, excluded) in [(800.0, spotify), (1200.0, brave)] {
                            let level = magnitude(&samples, hz);
                            if excluded {
                                assert!(level < 0.002, "excluded source reached durable replay");
                            } else {
                                assert!(
                                    level > 0.08,
                                    "eligible source missing from durable replay"
                                );
                            }
                        }
                    }
                }
            }
            if mode != Mode::Cloud {
                let chunks = inspected.0.lock().unwrap();
                assert!(
                    !chunks.is_empty(),
                    "fake local recognizer must inspect real admitted PCM: {:?}",
                    outcome.stt_errors
                );
                for (hz, excluded) in [(800.0, spotify), (1200.0, brave)] {
                    let level = chunks
                        .iter()
                        .map(|pcm| magnitude(pcm, hz))
                        .fold(0.0, f32::max);
                    if excluded {
                        assert!(level < 0.002, "excluded signal reached local inference");
                    } else {
                        assert!(level > 0.08, "eligible signal missing at local inference");
                    }
                }
            }
            if let Some(provider) = provider.as_mut() {
                tokio::time::timeout(Duration::from_secs(3), &mut provider.task)
                    .await
                    .expect("owned loopback provider must finish")
                    .unwrap();
                let uploads = provider.uploads.lock().unwrap();
                assert_eq!(
                    uploads.len(),
                    2,
                    "both system and microphone must reach their loopback speech connection"
                );
                for hz in [400.0, 1600.0] {
                    assert!(
                        uploads
                            .iter()
                            .map(|pcm| magnitude(pcm, hz))
                            .fold(0.0, f32::max)
                            > 0.08,
                        "eligible source missing from actual speech upload: {hz}Hz mode={mode:?} Spotify={spotify} Brave={brave} fail_reopen={fail_reopen} uploads={:?} errors={:?}",
                        uploads
                            .iter()
                            .map(|pcm| (pcm.len(), magnitude(pcm, 400.0), magnitude(pcm, 1600.0)))
                            .collect::<Vec<_>>(),
                        outcome.stt_errors
                    );
                }
                for (hz, excluded) in [(800.0, spotify), (1200.0, brave)] {
                    let level = uploads
                        .iter()
                        .map(|pcm| magnitude(pcm, hz))
                        .fold(0.0, f32::max);
                    if excluded {
                        assert!(
                            level < 0.002,
                            "excluded source reached actual speech upload: {hz}Hz={level}"
                        );
                    } else {
                        assert!(
                            level > 0.08,
                            "eligible source missing from actual speech upload"
                        );
                    }
                }
            }
            if mode == Mode::Fallback {
                let sources = control.speech.snapshot();
                assert_eq!(sources.len(), 2);
                assert!(
                    sources
                        .iter()
                        .all(|source| source.provider == fotw_stt::local::PROVIDER),
                    "outage must actually switch both sources to local inference: {sources:?}"
                );
            }
        }
    }
}

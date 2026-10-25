//! A real cloud handshake, durable canonical audio, and a fake local model.
use fotw_stt::{
    DeepgramEndpoint, DeepgramStreamConfig, Recognized, Recognizer, RecognizerFactory, Source,
    StreamEvent, SttError,
};
use fotwd::failover::coverage::{CoverageJournal, SwitchReason};
use fotwd::failover::stream::{Event, FailoverConfig, FailoverStream};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Factory;
struct Model;
struct ReadyOnceFactory(std::sync::atomic::AtomicBool);
impl RecognizerFactory for ReadyOnceFactory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        if self.0.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err(SttError::new(
                fotw_stt::SttErrorClass::Server,
                "fixture",
                "another native state cannot be allocated",
            ));
        }
        Ok(Box::new(Model))
    }
}
impl RecognizerFactory for Factory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(Model))
    }
}
impl Recognizer for Model {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: pcm.len() as u64 / 16,
            text: "yes yes please".into(),
        }])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_second_local_state_refuses_admission_before_any_cloud_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let factory: Arc<dyn RecognizerFactory> =
        Arc::new(ReadyOnceFactory(std::sync::atomic::AtomicBool::new(false)));
    let config = |source| {
        FailoverConfig::new(
            DeepgramStreamConfig::new(
                "fixture-not-a-key",
                fotw_stt::deepgram::DeepgramConfig::new("placeholder", source),
            )
            .with_endpoint(DeepgramEndpoint::loopback(port)),
            Arc::clone(&factory),
        )
    };
    assert!(
        fotwd::session::FailoverLegs::prepare(config(Source::System), Some(config(Source::Mic)))
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_pump_recovers_both_tracks_without_losing_original_capture_formats() {
    use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
    use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
    use fotw_pipeline::replay::{ReplayReader, ReplaySource};
    use fotwd::session::{self, FailoverLegs, Transcription};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepts = Arc::clone(&accepted);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            accepts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0; 1];
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8_192);
            }
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let config = |source| {
        let cloud = DeepgramStreamConfig::new(
            "fixture-not-a-key",
            fotw_stt::deepgram::DeepgramConfig::new("placeholder", source),
        )
        .with_endpoint(DeepgramEndpoint::loopback(port));
        FailoverConfig::new(cloud, Arc::new(Factory))
    };
    let legs = FailoverLegs::prepare(config(Source::System), Some(config(Source::Mic)))
        .await
        .unwrap();
    // Preparing local states must not open either cloud connection.
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 0);
    fn tap(rate: u32, channels: u16, source: Source) -> Box<dyn AudioTap> {
        let samples = (0..rate * 2)
            .flat_map(|i| {
                let amplitude = if (i / (rate / 20)).is_multiple_of(2) {
                    0.1
                } else {
                    0.6
                };
                let value =
                    (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * amplitude;
                std::iter::repeat_n(value, channels as usize)
            })
            .collect();
        let id = match source {
            Source::System => TapId::system_default(),
            Source::Mic => TapId::mic("fixture-mic"),
        };
        Box::new(FileAudioSource::from_wav(
            id,
            fotw_audio::wav::WavData {
                format: StreamFormat::new(rate, channels, SampleFormat::I16),
                samples,
            },
            ReplaySpeed::Realtime,
        ))
    }
    let root = tempfile::tempdir().unwrap();
    let outcome = session::run(
        root.path(),
        tap(48_000, 2, Source::System),
        Some(tap(44_100, 1, Source::Mic)),
        Transcription::Failover(Box::new(legs)),
        Duration::from_millis(1_200),
    )
    .await
    .unwrap();
    server.await.unwrap();
    let state = fotw_pipeline::wal::SessionState::read(&outcome.dir).unwrap();
    assert!(state.is_finalized());
    assert_eq!(
        state.manifest.system_format,
        Some(fotw_pipeline::wal::TrackFormat::new(48_000, 2))
    );
    assert_eq!(
        state.manifest.mic_format,
        Some(fotw_pipeline::wal::TrackFormat::new(44_100, 1))
    );
    assert_eq!(
        std::fs::metadata(outcome.dir.join("system.pcm"))
            .unwrap()
            .len(),
        outcome.system_samples * 2
    );
    assert_eq!(
        std::fs::metadata(outcome.dir.join("mic.pcm"))
            .unwrap()
            .len(),
        outcome.mic_samples * 2
    );
    assert!(outcome.system_samples > 48_000);
    assert!(outcome.mic_samples > 22_050);
    let session_id = outcome.dir.file_name().unwrap().to_str().unwrap();
    for (source, replay_source) in [
        (Source::System, ReplaySource::System),
        (Source::Mic, ReplaySource::Mic),
    ] {
        let finals: Vec<_> = outcome
            .segments
            .iter()
            .filter(|segment| segment.source == source)
            .collect();
        assert!(
            !finals.is_empty(),
            "no local result for {source:?}: {:?}",
            outcome.stt_errors
        );
        assert!(
            finals
                .iter()
                .all(|s| s.provider == fotw_stt::local::PROVIDER)
        );
        let mut reader = ReplayReader::open(&outcome.dir, session_id, replay_source).unwrap();
        assert!(reader.is_complete().unwrap());
        let mut cursor = reader.first_sample();
        let mut canonical = Vec::new();
        loop {
            let chunk = reader.read_from(cursor, 16_000).unwrap();
            if chunk.is_empty() {
                break;
            }
            cursor += chunk.len() as u64;
            canonical.extend(chunk);
        }
        assert!(
            canonical
                .iter()
                .filter(|sample| sample.unsigned_abs() > 10)
                .count()
                * 10
                > canonical.len() * 8,
            "{source:?} canonical replay replaced original speech with echo-gate silence"
        );
        let journal = CoverageJournal::open(&outcome.dir, session_id, source).unwrap();
        assert_eq!(journal.current_epoch().reason, Some(SwitchReason::Auth));
        assert!(
            finals
                .iter()
                .all(|segment| segment.session_id == session_id)
        );
    }
    let mut db =
        fotw_store::Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x28; 32])).unwrap();
    let meeting = fotwd::persist::persist_session(&mut db, &outcome, "Synthetic handoff").unwrap();
    let saved = db.export_meeting(&meeting).unwrap();
    assert_eq!(
        saved.speech_epochs.len(),
        4,
        "both source/provider histories must survive persistence"
    );
    assert!(!saved.speech_receipts.is_empty());
    assert_eq!(saved.segments.len(), outcome.segments.len());
    assert!(
        outcome
            .segments
            .iter()
            .all(|s| saved.segments.iter().any(|row| row.id == s.id))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_handoff_replays_more_than_thirty_seconds_and_preserves_the_session_clock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0; 1];
            socket.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8_192);
        }
        gate.notified().await;
        socket
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let cloud = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("s", Source::System),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port))
    .with_session_offset_ms(137);
    let mut config = FailoverConfig::new(
        cloud,
        Arc::new(ReadyOnceFactory(std::sync::atomic::AtomicBool::new(false))),
    );
    config.limits.speech_samples_without_final = 90 * 16_000;
    config.local_revision = Some("fixture-local-checksum".into());
    let dir = tempfile::tempdir().unwrap();
    let (stream, mut events) = FailoverStream::open(dir.path(), config).await.unwrap();
    let header: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(dir.path().join("system.stt-coverage.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        header["header"]["fallback"]["engine"]["model"],
        "fixture-local"
    );
    assert_eq!(
        header["header"]["fallback"]["engine"]["revision"],
        "fixture-local-checksum"
    );
    assert_eq!(header["header"]["fallback"]["language"], "en");
    let pcm: Vec<i16> = (0..73 * 16_000)
        .map(|n| if (n / 480) % 2 == 0 { 1_000 } else { 8_000 })
        .collect();
    stream.write(&pcm).unwrap();
    release.notify_one();
    let mut finals = Vec::new();
    let switched = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                Event::ProviderSwitch(epoch) => break epoch,
                Event::Transcript(StreamEvent::Final(segment)) => finals.push(segment),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(switched.reason, Some(SwitchReason::Auth));
    assert_eq!(switched.from_sample, 137 * 16);
    assert!(stream.finish().await, "{:?}", stream.status());
    while let Ok(event) = events.try_recv() {
        if let Event::Transcript(StreamEvent::Final(segment)) = event {
            finals.push(segment);
        }
    }
    assert!(!finals.is_empty());
    assert_eq!(finals[0].start_ms, 137);
    assert_eq!(finals.last().unwrap().end_ms, 137 + 73_000);
    assert!(
        finals
            .iter()
            .all(|s| s.provider == fotw_stt::local::PROVIDER)
    );
    assert_eq!(
        stream.status().committed_sample,
        137 * 16 + pcm.len() as u64
    );
    assert!(
        fotw_pipeline::replay::ReplayReader::open(
            dir.path(),
            "s",
            fotw_pipeline::replay::ReplaySource::System
        )
        .unwrap()
        .is_complete()
        .unwrap()
    );
    server.await.unwrap();
    drop(stream);
    let recovered = CoverageJournal::open(dir.path(), "s", Source::System).unwrap();
    assert_eq!(recovered.committed_through(), 137 * 16 + pcm.len() as u64);
    assert_eq!(
        recovered.commits().iter().flat_map(|c| &c.segments).count(),
        finals.len()
    );
}

struct BlockingFactory {
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    entered: Arc<std::sync::atomic::AtomicBool>,
}
struct BlockingModel(BlockingFactory);
struct ReleaseOnDrop(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
impl RecognizerFactory for BlockingFactory {
    fn model(&self) -> &str {
        "blocked-fixture"
    }
    fn language(&self) -> Option<&str> {
        None
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(BlockingModel(BlockingFactory {
            release: self.release.clone(),
            entered: self.entered.clone(),
        })))
    }
}
impl Recognizer for BlockingModel {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.0
            .entered
            .store(true, std::sync::atomic::Ordering::Release);
        let (lock, wake) = &*self.0.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: pcm.len() as u64 / 16,
            text: "must not publish after Stop".into(),
        }])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_is_bounded_and_fences_a_native_result_that_returns_after_its_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0; 1];
            socket.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8_192);
        }
        socket
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cloud = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("s", Source::System),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port));
    let mut config = FailoverConfig::new(
        cloud,
        Arc::new(BlockingFactory {
            release: release.clone(),
            entered: entered.clone(),
        }),
    );
    config.endpoint.hard_max_ms = 270;
    config.stop_timeout = Duration::from_millis(40);
    let dir = tempfile::tempdir().unwrap();
    let (stream, mut events) = FailoverStream::open(dir.path(), config).await.unwrap();
    let pcm: Vec<i16> = (0..16_000)
        .map(|n| if (n / 480) % 2 == 0 { 1_000 } else { 8_000 })
        .collect();
    stream.write(&pcm).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !entered.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(1), stream.finish())
        .await
        .unwrap();
    // Always release the owned fake native operation, including assertion failures.
    let pending = stream.status().worker_running;
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    assert!(!completed);
    assert!(pending);
    assert!(stream.write(&[1]).is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while stream.status().worker_running {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut closed = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            Event::Transcript(StreamEvent::Final(segment)) => {
                panic!("late final escaped: {}", segment.text)
            }
            Event::Transcript(StreamEvent::State(fotw_stt::StreamState::Closed)) => closed += 1,
            _ => {}
        }
    }
    assert_eq!(closed, 1);
    server.await.unwrap();
    drop(stream);
    let pending = CoverageJournal::open(dir.path(), "s", Source::System).unwrap();
    assert_eq!(pending.committed_through(), 0);
    assert!(
        !fotw_pipeline::replay::ReplayReader::open(
            dir.path(),
            "s",
            fotw_pipeline::replay::ReplaySource::System
        )
        .unwrap()
        .is_complete()
        .unwrap()
    );
}

async fn silent_provider() -> (
    DeepgramStreamConfig,
    Arc<std::sync::atomic::AtomicU64>,
    tokio::task::JoinHandle<()>,
) {
    use futures_util::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observed = received.clone();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = message {
                observed.fetch_add(
                    (bytes.len() / 2) as u64,
                    std::sync::atomic::Ordering::Release,
                );
            }
        }
    });
    let config = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("s", Source::System),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port));
    (config, received, server)
}

async fn wait_open(events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                Event::Transcript(StreamEvent::State(fotw_stt::StreamState::Open)) => break,
                Event::ProviderSwitch(epoch) => panic!("switched before fixture opened: {epoch:?}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connected_provider_without_finals_demotes_for_speech_and_replays_the_whole_pending_interval()
 {
    let (cloud, _, server) = silent_provider().await;
    let dir = tempfile::tempdir().unwrap();
    let (stream, mut events) =
        FailoverStream::open(dir.path(), FailoverConfig::new(cloud, Arc::new(Factory)))
            .await
            .unwrap();
    wait_open(&mut events).await;
    let pcm: Vec<i16> = (0..73 * 16_000)
        .map(|n| if (n / 480) % 2 == 0 { 1_000 } else { 8_000 })
        .collect();
    stream.write(&pcm).unwrap();
    let epoch = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::ProviderSwitch(epoch) = events.recv().await.unwrap() {
                break epoch;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(epoch.reason, Some(SwitchReason::SpeechStall));
    assert_eq!(epoch.from_sample, 0);
    assert!(stream.finish().await, "{:?}", stream.status());
    assert_eq!(stream.status().committed_sample, pcm.len() as u64);
    assert_eq!(stream.status().lag_samples, 0);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_minute_of_silence_stays_on_cloud_and_stop_accounts_for_the_quiet_tail() {
    let (cloud, received, server) = silent_provider().await;
    let dir = tempfile::tempdir().unwrap();
    let (stream, mut events) =
        FailoverStream::open(dir.path(), FailoverConfig::new(cloud, Arc::new(Factory)))
            .await
            .unwrap();
    wait_open(&mut events).await;
    stream.write(&vec![0; 60 * 16_000]).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.load(std::sync::atomic::Ordering::Acquire) < 60 * 16_000 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(stream.status().phase, fotwd::failover::stream::Phase::Cloud);
    assert!(stream.finish().await, "{:?}", stream.status());
    assert_eq!(stream.status().committed_sample, 60 * 16_000);
    while let Ok(event) = events.try_recv() {
        match event {
            Event::Transcript(StreamEvent::Final(segment)) => {
                panic!("silence produced text: {}", segment.text)
            }
            Event::ProviderSwitch(epoch) => {
                assert_eq!(epoch.reason, Some(SwitchReason::FinishingTail))
            }
            _ => {}
        }
    }
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

/// Opt-in installed-model smoke. Only the checksum-verified public JFK fixture
/// is accepted; this never opens a capture device or a paid provider connection.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires cached public small.en weights and checksum-verified JFK WAV"]
async fn installed_whisper_recovers_a_public_clip_after_reconnect_budget_exhaustion() {
    use sha2::{Digest, Sha256};
    let model = std::path::PathBuf::from(
        std::env::var_os("FOTW_NATIVE_HANDOFF_MODEL").expect("set FOTW_NATIVE_HANDOFF_MODEL"),
    );
    let wav = std::path::PathBuf::from(
        std::env::var_os("FOTW_NATIVE_HANDOFF_WAV").expect("set FOTW_NATIVE_HANDOFF_WAV"),
    );
    let checked_model = model.clone();
    tokio::task::spawn_blocking(move || {
        let known = fotwd::stt::verify_model(&checked_model)
            .unwrap()
            .expect("only published known weights are allowed");
        assert_eq!(known.name, "small.en");
    })
    .await
    .unwrap();
    let bytes = std::fs::read(wav).unwrap();
    assert_eq!(
        Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e",
        "this smoke accepts only the public JFK fixture, never user audio"
    );
    let mut offset = 12;
    let pcm = loop {
        let length = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let start = offset + 8;
        if &bytes[offset..offset + 4] == b"data" {
            break bytes[start..start + length]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>();
        }
        offset = start + length + length % 2;
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for _ in 0..4 {
            // initial failure plus three failed reconnect cycles.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0; 1];
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8_192);
            }
            socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let cloud = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("s", Source::System),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port))
    .with_backoff(fotw_stt::BackoffPolicy::fast());
    let engine = Arc::new(fotw_stt::WhisperEngine::new(model, Some("en".into())));
    let mut config = FailoverConfig::new(cloud, engine.clone());
    config.local_revision =
        Some("sha256:c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d".into());
    let dir = tempfile::tempdir().unwrap();
    let began = std::time::Instant::now();
    let (stream, mut events) = FailoverStream::open(dir.path(), config).await.unwrap();
    stream.write(&pcm).unwrap();
    let mut finals = Vec::new();
    let epoch = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap() {
                Event::ProviderSwitch(epoch) => break epoch,
                Event::Transcript(StreamEvent::Final(segment)) => finals.push(segment),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(epoch.reason, Some(SwitchReason::TransportBudget));
    assert!(stream.finish().await, "{:?}", stream.status());
    while let Ok(event) = events.try_recv() {
        if let Event::Transcript(StreamEvent::Final(segment)) = event {
            finals.push(segment);
        }
    }
    let transcript = finals
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        transcript.to_ascii_lowercase().contains("country"),
        "{transcript}"
    );
    assert_eq!(stream.status().committed_sample, pcm.len() as u64);
    assert_eq!(stream.status().lag_samples, 0);
    eprintln!(
        "{}",
        serde_json::json!({"fixture":"public-jfk-sha256-verified", "elapsed_ms":began.elapsed().as_millis(),
        "reason":epoch.reason, "model":epoch.engine.model, "revision":epoch.engine.revision,
        "samples":pcm.len(), "transcript":transcript})
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    // Reopen a separately persisted pre-demotion source using only its saved
    // local plan and the verified installed model. No cloud transport exists.
    let plan = CoverageJournal::snapshot(dir.path(), "s", Source::System)
        .unwrap()
        .fallback
        .unwrap();
    let pending = tempfile::tempdir().unwrap();
    let mut writer = fotw_pipeline::replay::ReplayWriter::create(
        pending.path(),
        "native-restart",
        fotw_pipeline::replay::ReplaySource::System,
        0,
    )
    .unwrap();
    writer.append(&pcm).unwrap();
    writer.sync().unwrap();
    drop(writer);
    let journal = CoverageJournal::create_with_fallback(
        pending.path(),
        "native-restart",
        Source::System,
        fotwd::failover::coverage::EngineDescriptor {
            provider: "deepgram".into(),
            model: "nova-3".into(),
            revision: None,
            capabilities: fotw_stt::SttCapabilities::deepgram(),
        },
        Some(plan.clone()),
    )
    .unwrap();
    drop(journal);
    let began = std::time::Instant::now();
    let (recovered, mut events) = FailoverStream::resume_local(
        pending.path(),
        fotwd::failover::stream::LocalResumeConfig {
            session_id: "native-restart".into(),
            source: Source::System,
            local: engine,
            revision: plan.engine.revision,
            stop_timeout: Duration::from_secs(30),
        },
    )
    .await
    .unwrap();
    assert!(recovered.finish().await, "{:?}", recovered.status());
    let mut text = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::Transcript(StreamEvent::Final(segment)) = event {
            text.push(segment.text);
        }
    }
    let transcript = text.join(" ");
    assert!(
        transcript.to_ascii_lowercase().contains("country"),
        "{transcript}"
    );
    assert_eq!(recovered.status().committed_sample, pcm.len() as u64);
    assert_eq!(
        recovered.status().epoch.reason,
        Some(SwitchReason::ResumePending)
    );
    eprintln!(
        "{}",
        serde_json::json!({"fixture":"public-jfk-sha256-verified","mode":"local-restart","elapsed_ms":began.elapsed().as_millis(),"samples":pcm.len(),"transcript":transcript})
    );
}

struct PendingFixture {
    _root: tempfile::TempDir,
    dir: std::path::PathBuf,
    session: String,
    anchor: u64,
    end: u64,
    outcome: fotwd::SessionOutcome,
}

fn pending_fixture(seal: bool) -> PendingFixture {
    use fotw_pipeline::replay::{ReplaySource, ReplayWriter};
    use fotwd::failover::coverage::{EngineDescriptor, LocalPlan};
    let root = tempfile::tempdir().unwrap();
    let mut wal = fotw_pipeline::wal::SessionWal::create(root.path(), 16_000, 1).unwrap();
    let dir = wal.dir().to_owned();
    let session = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let pcm: Vec<i16> = (0u32..73 * 16_000)
        .map(|n| {
            if (n / 3_200).is_multiple_of(2) {
                1000
            } else {
                8000
            }
        })
        .collect();
    wal.write_system(
        &pcm.iter()
            .map(|v| f32::from(*v) / 32768.0)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    wal.finalize().unwrap();
    let anchor = 137 * 16;
    let mut writer = ReplayWriter::create(&dir, &session, ReplaySource::System, anchor).unwrap();
    writer.append(&pcm).unwrap();
    writer.sync().unwrap();
    if seal {
        writer.seal().unwrap();
    }
    let end = writer.end_sample();
    let factory: Arc<dyn RecognizerFactory> = Arc::new(Factory);
    let local = FailoverConfig::new(
        DeepgramStreamConfig::new(
            "unused-fixture",
            fotw_stt::deepgram::DeepgramConfig::new(&session, Source::System),
        ),
        factory,
    );
    let plan = LocalPlan {
        engine: EngineDescriptor {
            provider: fotw_stt::local::PROVIDER.into(),
            model: "fixture-local".into(),
            revision: Some("fixture-local-checksum".into()),
            capabilities: local.local_capabilities,
        },
        language: Some("en".into()),
        endpoint: local.endpoint,
    };
    let mut journal = CoverageJournal::create_with_fallback(
        &dir,
        &session,
        Source::System,
        EngineDescriptor {
            provider: "deepgram".into(),
            model: "nova-3".into(),
            revision: None,
            capabilities: fotw_stt::SttCapabilities::deepgram(),
        },
        Some(plan),
    )
    .unwrap();
    let mut original =
        fotw_stt::TranscriptSegment::new(&session, Source::System, "deepgram", "nova-3");
    original.id = "stable-original-prefix".into();
    original.text = "original committed prefix".into();
    original.is_final = true;
    original.start_ms = 137;
    original.end_ms = 10_137;
    journal
        .commit(0, anchor, anchor + 10 * 16_000, vec![original.clone()])
        .unwrap();
    drop(journal);
    drop(writer);
    let outcome = fotwd::SessionOutcome {
        dir: dir.clone(),
        started_at_ms: 1_000,
        capture_ended_at_ms: Some(74_137),
        system_samples: pcm.len() as u64,
        segments: vec![original],
        ..Default::default()
    };
    PendingFixture {
        _root: root,
        dir,
        session,
        anchor,
        end,
        outcome,
    }
}
fn resume_request(fixture: &PendingFixture) -> fotwd::failover::stream::LocalResumeConfig {
    fotwd::failover::stream::LocalResumeConfig {
        session_id: fixture.session.clone(),
        source: Source::System,
        local: Arc::new(Factory),
        revision: Some("fixture-local-checksum".into()),
        stop_timeout: Duration::from_secs(5),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_restart_recovers_only_pending_audio_and_projects_once_without_replacing_edits() {
    for sealed in [false, true] {
        let fixture = pending_fixture(sealed);
        let key = fotw_store::DbKey::from_bytes([0x29; 32]);
        let database = fixture._root.path().join("owned.sqlite3");
        let mut db = fotw_store::Db::open(&database, &key).unwrap();
        let meeting =
            fotwd::persist::persist_session(&mut db, &fixture.outcome, "Owned recovery").unwrap();
        db.conn()
            .execute(
                "UPDATE segments SET text='owned correction' WHERE id='stable-original-prefix'",
                [],
            )
            .unwrap();
        drop(db);
        let original_pcm = std::fs::read(fixture.dir.join("system.pcm")).unwrap();
        let original_canonical = std::fs::read(fixture.dir.join("system.stt16")).unwrap();
        let (stream, mut events) =
            FailoverStream::resume_local(&fixture.dir, resume_request(&fixture))
                .await
                .unwrap();
        assert!(
            stream.write(&[123]).is_err(),
            "resumed input must stay frozen"
        );
        assert!(stream.finish().await, "{:?}", stream.status());
        assert_eq!(stream.status().written_sample, fixture.end);
        assert_eq!(stream.status().committed_sample, fixture.end);
        let mut finals = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let Event::Transcript(StreamEvent::Final(segment)) = event {
                finals.push(segment);
            }
        }
        assert!(!finals.is_empty());
        assert!(
            finals
                .iter()
                .all(|s| s.start_ms >= 10_137 && s.provider == fotw_stt::local::PROVIDER)
        );
        assert_eq!(finals.last().unwrap().end_ms, 73_137);
        drop(stream);
        assert_eq!(
            std::fs::read(fixture.dir.join("system.pcm")).unwrap(),
            original_pcm
        );
        assert_eq!(
            std::fs::read(fixture.dir.join("system.stt16")).unwrap(),
            original_canonical
        );
        let snapshot =
            CoverageJournal::snapshot(&fixture.dir, &fixture.session, Source::System).unwrap();
        assert_eq!(snapshot.epochs.len(), 2);
        assert_eq!(snapshot.epochs[1].from_sample, fixture.anchor + 10 * 16_000);
        assert_eq!(snapshot.epochs[1].reason, Some(SwitchReason::ResumePending));
        let mut db = fotw_store::Db::open(&database, &key).unwrap();
        assert_eq!(
            fotwd::persist::persist_session(&mut db, &fixture.outcome, "Ignored retry title")
                .unwrap(),
            meeting
        );
        let saved = db.export_meeting(&meeting).unwrap();
        assert_eq!(
            saved
                .segments
                .iter()
                .filter(|s| s.id == "stable-original-prefix")
                .count(),
            1
        );
        assert_eq!(
            saved
                .segments
                .iter()
                .find(|s| s.id == "stable-original-prefix")
                .unwrap()
                .text,
            "owned correction"
        );
        assert_eq!(saved.segments.len(), finals.len() + 1);
        assert_eq!(saved.speech_epochs.len(), 2);
        // Another recovery attempt has nothing to infer and adds no fake switch.
        let (complete, _events) =
            FailoverStream::resume_local(&fixture.dir, resume_request(&fixture))
                .await
                .unwrap();
        assert!(complete.finish().await);
        drop(complete);
        assert_eq!(
            fotwd::persist::persist_session(&mut db, &fixture.outcome, "Ignored retry").unwrap(),
            meeting
        );
        assert_eq!(db.export_meeting(&meeting).unwrap(), saved);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_restart_refuses_changed_model_revision_or_language_and_retains_pending_input() {
    struct WrongFactory(&'static str, Option<&'static str>);
    impl RecognizerFactory for WrongFactory {
        fn model(&self) -> &str {
            self.0
        }
        fn language(&self) -> Option<&str> {
            self.1
        }
        fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
            panic!("mismatch must be refused before native allocation")
        }
    }
    for mismatch in ["model", "revision", "language"] {
        let fixture = pending_fixture(false);
        let paths = [
            fixture.dir.join("system.stt16"),
            fixture.dir.join("system.stt-coverage.jsonl"),
        ];
        let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let mut request = resume_request(&fixture);
        match mismatch {
            "model" => request.local = Arc::new(WrongFactory("different-model", Some("en"))),
            "revision" => request.revision = Some("different-checksum".into()),
            "language" => request.local = Arc::new(WrongFactory("fixture-local", Some("fr"))),
            _ => unreachable!(),
        }
        assert!(
            FailoverStream::resume_local(&fixture.dir, request)
                .await
                .is_err()
        );
        for (path, bytes) in paths.iter().zip(before) {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert!(!fixture.dir.join("system.stt-sealed.json").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_restart_refuses_a_still_owned_source_and_unavailable_native_state() {
    use fotw_pipeline::replay::{ReplaySource, ReplayWriter};
    let fixture = pending_fixture(false);
    let owner = ReplayWriter::resume(&fixture.dir, &fixture.session, ReplaySource::System).unwrap();
    assert!(
        FailoverStream::resume_local(&fixture.dir, resume_request(&fixture))
            .await
            .is_err()
    );
    drop(owner);
    struct Unavailable;
    impl RecognizerFactory for Unavailable {
        fn model(&self) -> &str {
            "fixture-local"
        }
        fn language(&self) -> Option<&str> {
            Some("en")
        }
        fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
            Err(SttError::new(
                fotw_stt::SttErrorClass::Server,
                "fixture",
                "native model unavailable",
            ))
        }
    }
    let before = std::fs::read(fixture.dir.join("system.stt-coverage.jsonl")).unwrap();
    let mut request = resume_request(&fixture);
    request.local = Arc::new(Unavailable);
    assert!(
        FailoverStream::resume_local(&fixture.dir, request)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(fixture.dir.join("system.stt-coverage.jsonl")).unwrap(),
        before
    );
    assert!(!fixture.dir.join("system.stt-complete.json").exists());
}

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
    let dir = tempfile::tempdir().unwrap();
    let (stream, mut events) = FailoverStream::open(dir.path(), config).await.unwrap();
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
    let mut config = FailoverConfig::new(cloud, engine);
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
}

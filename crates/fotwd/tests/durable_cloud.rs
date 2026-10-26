//! Processed cloud input retains exact provenance without selecting a local model.
use fotw_pipeline::{
    aec::{BypassReason, Output, OutputKind},
    replay::{ReplayReader, ReplaySource},
};
use fotw_stt::{DeepgramEndpoint, DeepgramStreamConfig, Source};
use fotwd::failover::{
    coverage::CoverageJournal,
    processing::{self, Descriptor},
    stream::{CloudSourceConfig, FailoverStream, Phase},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_cloud_only_stream_survives_the_fallback_speech_budget_and_accepts_later_finals() {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_final = release.clone();
    let total = 73 * 16000;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        while count.load(Ordering::Acquire) < total {
            match socket.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    count.fetch_add(bytes.len() / 2, Ordering::AcqRel);
                }
                Some(Ok(Message::Close(_))) | None => return,
                Some(Err(_)) => return,
                _ => {}
            }
        }
        release_final.notified().await;
        socket.send(Message::text(serde_json::json!({"type":"Results","channel_index":[0,1],"start":0.0,"duration":73.0,"is_final":true,"speech_final":true,"channel":{"alternatives":[{"transcript":"cloud","confidence":1.0,"words":[{"word":"cloud","start":0.0,"end":73.0,"confidence":1.0}]}]}}).to_string())).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
    });
    let cloud = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("fixture", Source::Mic),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port));
    let mut config = CloudSourceConfig::new(cloud);
    config.stop_timeout = Duration::from_secs(2);
    let root = tempfile::tempdir().unwrap();
    let (stream, mut events) = FailoverStream::open_cloud(root.path(), config)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(fotwd::failover::stream::Event::Transcript(fotw_stt::StreamEvent::State(
                fotw_stt::StreamState::Open,
            ))) = events.recv().await
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let pcm: Vec<i16> = (0..total)
        .map(|i| if (i / 480) % 2 == 0 { 1000 } else { 8000 })
        .collect();
    stream.write(&pcm).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.load(Ordering::Acquire) < total && stream.status().phase == Phase::Cloud {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        stream.status().phase,
        Phase::Cloud,
        "fallback speech thresholds cannot terminate a healthy selected cloud source: {:?}",
        stream.status()
    );
    assert_eq!(received.load(Ordering::Acquire), total);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        while stream.status().committed_sample < total as u64 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(stream.finish().await, "{:?}", stream.status());
    let saved = CoverageJournal::snapshot(root.path(), "fixture", Source::Mic).unwrap();
    assert!(saved.fallback.is_none());
    assert_eq!(saved.epochs.len(), 1);
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_cloud_credentials_retain_processed_input_without_an_implicit_local_plan() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).await.unwrap();
            header.push(byte[0]);
            assert!(header.len() < 8192);
        }
        socket
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let cloud = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("fixture", Source::Mic),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port))
    .with_session_offset_ms(137);
    let mut config = CloudSourceConfig::new(cloud);
    config.processing = Some(Descriptor {
        backend: "fixture-aec".into(),
        version: "1".into(),
        profile: "preserve-original".into(),
    });
    config.stop_timeout = Duration::from_secs(2);
    config.cloud_settle = Duration::from_millis(20);
    let (stream, _) = FailoverStream::open_cloud(root.path(), config)
        .await
        .expect("processed cloud-only input must open without any local factory");
    let output = Output {
        epoch: 1,
        start_sample: 2192,
        samples: vec![0.25; 317],
        kind: OutputKind::Raw(BypassReason::MissingReference),
    };
    stream.write_processed(&output).unwrap();
    assert!(!stream.finish().await);
    assert_eq!(stream.status().phase, Phase::Incomplete);
    let status = stream.status_reader();
    drop(stream);
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while status.read().worker_running {
        assert!(std::time::Instant::now() < until);
        tokio::task::yield_now().await;
    }
    let saved = CoverageJournal::snapshot(root.path(), "fixture", Source::Mic).unwrap();
    assert!(
        saved.fallback.is_none(),
        "cloud-only cannot invent a local recovery plan"
    );
    assert_eq!(saved.epochs.len(), 1);
    assert_eq!(saved.epochs[0].engine.provider, "deepgram");
    let mut reader = ReplayReader::open(root.path(), "fixture", ReplaySource::Mic).unwrap();
    let retained = reader.read_from(2192, 317).unwrap();
    assert_eq!(retained, vec![8191; 317]);
    let evidence = processing::validate(root.path(), "fixture", ReplaySource::Mic, &mut reader)
        .unwrap()
        .unwrap();
    assert_eq!(evidence.end_sample, 2509);
    assert_eq!(evidence.raw_samples, 317);
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_only_stop_still_ends_its_settle_window_with_unconfirmed_input_retained() {
    use fotw_stt::{StreamEvent, StreamState};
    use fotwd::failover::stream::Event;
    use futures_util::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
    });
    let cloud = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("fixture", Source::Mic),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port));
    let mut config = CloudSourceConfig::new(cloud);
    config.cloud_settle = Duration::from_millis(20);
    config.stop_timeout = Duration::from_secs(1);
    let root = tempfile::tempdir().unwrap();
    let (stream, mut events) = FailoverStream::open_cloud(root.path(), config)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Event::Transcript(StreamEvent::State(StreamState::Open))) =
                events.recv().await
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    stream.write(&[1000; 1600]).unwrap();
    assert!(!stream.finish().await);
    let status = stream.status();
    assert_eq!(status.phase, Phase::Incomplete);
    assert!(
        status.error.unwrap().contains("no local fallback"),
        "Stop must finish its settle deadline rather than wait for the outer timeout"
    );
    let mut reader = ReplayReader::open(root.path(), "fixture", ReplaySource::Mic).unwrap();
    assert_eq!(reader.read_from(0, 1600).unwrap(), [1000; 1600]);
    assert!(
        CoverageJournal::snapshot(root.path(), "fixture", Source::Mic)
            .unwrap()
            .fallback
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

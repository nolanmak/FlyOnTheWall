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

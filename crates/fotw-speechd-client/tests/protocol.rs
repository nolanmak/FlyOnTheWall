//! #243 protocol acceptance, against a helper with no model and no network.
//!
//! By default these spawn `fotw-speechd-fake`. With `FOTW_SPEECHD_BIN` set they
//! run against that binary instead, in `--fake-engines` mode, so the Swift
//! helper is held to the same contract. Tests that need a fake-only knob (a
//! helper that lies about its version or sends garbage) skip themselves then.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fotw_speechd_client::frame::{self, Frame, KIND_AUDIO, KIND_CONTROL, MAX_FRAME};
use fotw_speechd_client::message::{CloseReason, ErrorCode, PROTOCOL_VERSION, Reply};
use fotw_speechd_client::{
    ClientError, RestartPolicy, SpeechStream, SpeechdClient, SpeechdConfig, StreamConfig,
    StreamEvent,
};

fn helper() -> (PathBuf, bool) {
    match std::env::var_os("FOTW_SPEECHD_BIN") {
        Some(path) => (PathBuf::from(path), false),
        None => (PathBuf::from(env!("CARGO_BIN_EXE_fotw-speechd-fake")), true),
    }
}

fn config(extra: &[&str]) -> SpeechdConfig {
    let mut config = SpeechdConfig::new(helper().0, std::env::temp_dir()).arg("--fake-engines");
    for arg in extra {
        config = config.arg(arg);
    }
    config
}

/// Fake-only knobs; `None` means skip under `FOTW_SPEECHD_BIN`.
fn fake_only(extra: &[&str]) -> Option<SpeechdConfig> {
    helper().1.then(|| config(extra))
}

fn stream(engine: &str) -> StreamConfig {
    StreamConfig::new(engine, 16_000)
}

/// The next event, failing the test instead of hanging on it.
async fn next(stream: &mut SpeechStream) -> Option<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .expect("event within 5 s")
}

/// Drain a finished stream to its final text.
async fn final_text(stream: &mut SpeechStream) -> String {
    let mut text = None;
    while let Some(event) = next(stream).await {
        match event {
            StreamEvent::Final(f) => text = Some(f.text),
            StreamEvent::Closed(CloseReason::Finished) => break,
            StreamEvent::Partial { .. } | StreamEvent::Vad { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    text.expect("a final before close")
}

// ------------------------------------------------------------------ raw pipe

/// The helper side of the contract, spoken to byte by byte.
struct Raw {
    child: std::process::Child,
}

impl Raw {
    fn spawn() -> Self {
        let child = Command::new(helper().0)
            .args(["serve", "--models-root"])
            .arg(std::env::temp_dir())
            .arg("--fake-engines")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        Self { child }
    }

    fn send(&mut self, bytes: &[u8]) {
        // The helper may already have hung up on an earlier bad frame.
        let _ = self.child.stdin.as_mut().unwrap().write_all(bytes);
        let _ = self.child.stdin.as_mut().unwrap().flush();
    }

    fn send_json(&mut self, value: serde_json::Value) {
        self.send(&frame::encode_control(&value).unwrap());
    }

    fn reply(&mut self) -> Reply {
        match frame::read_frame(self.child.stdout.as_mut().unwrap()) {
            Ok(Some(Frame::Control(json))) => serde_json::from_slice(&json).expect("a reply"),
            other => panic!("expected a control frame, got {other:?}"),
        }
    }

    fn hello(&mut self) {
        self.send_json(
            serde_json::json!({"type":"hello","id":1,"protocol":PROTOCOL_VERSION,"client":"raw"}),
        );
        assert!(matches!(
            self.reply(),
            Reply::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
    }

    /// Exit status, failing the test if the helper lingers.
    fn exit_code(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(Instant::now() < deadline, "helper did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_refused_with_protocol_error(&mut self) {
        match self.reply() {
            Reply::Error {
                code: ErrorCode::Protocol,
                fatal: true,
                ..
            } => {}
            other => panic!("expected a fatal protocol error, got {other:?}"),
        }
        assert_eq!(self.exit_code(), Some(65));
        let mut rest = Vec::new();
        self.child
            .stdout
            .as_mut()
            .unwrap()
            .read_to_end(&mut rest)
            .unwrap();
        assert!(rest.is_empty(), "nothing after the fatal error");
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn helper_refuses_a_client_of_another_protocol_version() {
    let mut raw = Raw::spawn();
    raw.send_json(serde_json::json!({"type":"hello","id":1,"protocol":99,"client":"future"}));
    match raw.reply() {
        Reply::Error {
            id: Some(1),
            code: ErrorCode::VersionMismatch,
            supported,
            ..
        } => {
            assert_eq!(supported, vec![PROTOCOL_VERSION]);
        }
        other => panic!("expected version_mismatch, got {other:?}"),
    }
    assert_eq!(raw.exit_code(), Some(0));
}

#[test]
fn helper_refuses_anything_before_hello() {
    let mut raw = Raw::spawn();
    raw.send_json(serde_json::json!({"type":"health","id":1}));
    raw.assert_refused_with_protocol_error();
}

#[test]
fn helper_refuses_an_oversized_frame_without_reading_it() {
    let mut raw = Raw::spawn();
    raw.hello();
    let mut header = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes().to_vec();
    header.push(KIND_AUDIO);
    // Only the header is sent: the refusal cannot be waiting for the payload.
    raw.send(&header);
    raw.assert_refused_with_protocol_error();
}

#[test]
fn helper_refuses_malformed_frames() {
    let not_json = [&[0, 0, 0, 3, KIND_CONTROL][..], b"{x"].concat();
    let unknown_type =
        frame::encode_control(&serde_json::json!({"type":"teleport","id":2})).unwrap();
    let missing_field = frame::encode_control(&serde_json::json!({"type":"load","id":2})).unwrap();
    let unknown_kind = vec![0, 0, 0, 2, 0x7f, 0];
    let odd_audio = vec![0, 0, 0, 6, KIND_AUDIO, 0, 0, 0, 1, 9];
    let audio_for_no_stream = frame::encode_audio(42, &[1, 2, 3]).unwrap();
    for bad in [
        not_json,
        unknown_type,
        missing_field,
        unknown_kind,
        odd_audio,
        audio_for_no_stream,
    ] {
        let mut raw = Raw::spawn();
        raw.hello();
        raw.send(&bad);
        raw.assert_refused_with_protocol_error();
    }
}

// ------------------------------------------------------------------ client

#[tokio::test]
async fn client_refuses_a_helper_of_another_protocol_version() {
    let Some(config) = fake_only(&["--announce-protocol", "2"]) else {
        return;
    };
    let started = Instant::now();
    match SpeechdClient::start(config).await {
        Err(ClientError::VersionMismatch { client, helper, .. }) => {
            assert_eq!((client, helper), (PROTOCOL_VERSION, Some(2)));
        }
        other => panic!("expected VersionMismatch, got {:?}", other.map(|_| ())),
    }
    // Refused outright: no restart loop behind it.
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn client_fails_open_streams_on_a_malformed_or_oversized_frame_from_the_helper() {
    for misbehaviour in ["oversized", "not-json", "unknown-type", "audio"] {
        let Some(config) = fake_only(&["--misbehave-on-health", misbehaviour]) else {
            return;
        };
        let client = SpeechdClient::start(config).await.expect("start");
        client.load("fake-stream").await.expect("load");
        let mut open = client
            .open_stream(stream("fake-stream"))
            .await
            .expect("open");
        match client.health().await {
            Err(ClientError::Protocol(_)) => {}
            other => panic!("{misbehaviour}: expected a protocol error, got {other:?}"),
        }
        match next(&mut open).await {
            Some(StreamEvent::Failed(failure)) => {
                assert_eq!(failure.code, ErrorCode::Protocol, "{misbehaviour}");
            }
            other => panic!("{misbehaviour}: expected the stream to fail, got {other:?}"),
        }
        assert_eq!(next(&mut open).await, None);
        client.shutdown().await;
    }
}

#[tokio::test]
async fn two_concurrent_streams_cancel_independently() {
    let client = SpeechdClient::start(config(&[])).await.expect("start");
    client.load("fake-stream").await.expect("load");
    let mut a = client
        .open_stream(stream("fake-stream"))
        .await
        .expect("open a");
    let mut b = client
        .open_stream(stream("fake-stream"))
        .await
        .expect("open b");
    assert_ne!(a.id(), b.id());

    a.send_audio(&[1; 160]).await.expect("audio a");
    b.send_audio(&[2; 320]).await.expect("audio b");
    assert!(
        matches!(next(&mut a).await, Some(StreamEvent::Partial { ref text, .. }) if text == "samples=160")
    );
    assert!(
        matches!(next(&mut b).await, Some(StreamEvent::Partial { ref text, .. }) if text == "samples=320")
    );

    a.cancel();
    assert_eq!(
        next(&mut a).await,
        None,
        "a cancelled stream yields nothing more"
    );
    assert!(a.send_audio(&[1; 160]).await.is_err(), "nor accepts audio");
    // The helper dropped a, and only a.
    assert_eq!(client.health().await.expect("health").streams, 1);

    b.send_audio(&[2; 480]).await.expect("b keeps going");
    b.finish().await.expect("finish b");
    assert_eq!(final_text(&mut b).await, "samples=800");
    assert_eq!(client.health().await.expect("health").streams, 0);
    client.shutdown().await;
}

#[tokio::test]
async fn a_crash_fails_open_streams_within_a_second_and_the_next_stream_succeeds() {
    let started = Instant::now();
    let client = SpeechdClient::start(config(&[])).await.expect("start");
    client.load("fake-batch").await.expect("load");
    client.load("fake-crash").await.expect("load");
    let mut bystander = client
        .open_stream(stream("fake-batch"))
        .await
        .expect("open");
    let mut doomed = client
        .open_stream(stream("fake-crash"))
        .await
        .expect("open");

    let crashed_at = Instant::now();
    doomed
        .send_audio(&[1; 160])
        .await
        .expect("audio reaches the helper");
    for open in [&mut doomed, &mut bystander] {
        match next(open).await {
            Some(StreamEvent::Failed(failure)) => {
                assert_eq!(failure.code, ErrorCode::HelperCrashed)
            }
            other => panic!("expected helper_crashed, got {other:?}"),
        }
        assert!(
            crashed_at.elapsed() < Duration::from_secs(1),
            "failed within 1 s"
        );
        assert_eq!(next(open).await, None);
    }

    // The client restarts the helper, replays the loads, and serves again.
    let mut after = client
        .open_stream(stream("fake-batch"))
        .await
        .expect("open after restart");
    after.send_audio(&[3; 1600]).await.expect("audio");
    after.finish().await.expect("finish");
    assert_eq!(final_text(&mut after).await, "samples=1600");
    assert!(
        started.elapsed() >= RestartPolicy::default().min_interval,
        "spawns at least 2 s apart"
    );
    let health = client.health().await.expect("health");
    assert!(
        health.loaded.contains(&"fake-batch".to_owned()),
        "loads replayed: {health:?}"
    );
    assert_eq!(
        client.status().restart_attempts,
        0,
        "a finished stream resets the budget"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn a_hung_helper_is_killed_and_fails_its_streams_instead_of_blocking_them() {
    let mut config = config(&[]);
    config.heartbeat_interval = Duration::from_millis(100);
    config.request_timeout = Duration::from_millis(400);
    let client = SpeechdClient::start(config).await.expect("start");
    client.load("fake-hang").await.expect("load");
    let mut stuck = client.open_stream(stream("fake-hang")).await.expect("open");
    stuck.send_audio(&[1; 16]).await.expect("audio");
    stuck.finish().await.expect("finish is queued, not awaited");
    let waited = Instant::now();
    match next(&mut stuck).await {
        Some(StreamEvent::Failed(failure)) => {
            assert_eq!(failure.code, ErrorCode::HelperUnresponsive)
        }
        other => panic!("expected helper_unresponsive, got {other:?}"),
    }
    assert!(
        waited.elapsed() < Duration::from_secs(1),
        "found within a heartbeat and a deadline"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn restarts_stop_after_three_attempts_until_reset() {
    let mut config = config(&[]);
    config.restart.min_interval = Duration::from_millis(50);
    let client = SpeechdClient::start(config).await.expect("start");
    client.load("fake-batch").await.expect("load");
    client.load("fake-crash").await.expect("load");
    for attempt in 0..=3 {
        let mut doomed = client
            .open_stream(stream("fake-crash"))
            .await
            .unwrap_or_else(|e| {
                panic!("attempt {attempt}: {e:?}");
            });
        doomed.send_audio(&[1; 16]).await.expect("audio");
        assert!(matches!(
            next(&mut doomed).await,
            Some(StreamEvent::Failed(_))
        ));
    }
    match client.open_stream(stream("fake-batch")).await {
        Err(ClientError::HelperFailed { attempts: 3 }) => {}
        other => panic!(
            "expected HelperFailed after 3 restarts, got {:?}",
            other.map(|s| s.id())
        ),
    }
    assert!(client.status().failed);
    client.reset().await;
    let mut ok = client
        .open_stream(stream("fake-batch"))
        .await
        .expect("works after reset");
    ok.send_audio(&[1; 10]).await.expect("audio");
    ok.finish().await.expect("finish");
    assert_eq!(final_text(&mut ok).await, "samples=10");
    client.shutdown().await;
}

#[tokio::test]
async fn the_client_validates_engine_format_and_frame_size_before_sending() {
    let client = SpeechdClient::start(config(&[])).await.expect("start");
    assert!(matches!(
        client.open_stream(stream("no-such-engine")).await,
        Err(ClientError::UnknownEngine(_))
    ));
    assert!(matches!(
        client
            .open_stream(StreamConfig::new("fake-batch", 44_100))
            .await,
        Err(ClientError::UnsupportedFormat(_))
    ));
    client.load("fake-batch").await.expect("load");
    let mut big = client
        .open_stream(stream("fake-batch"))
        .await
        .expect("open");
    // Three frames' worth: the client splits it rather than sending one
    // oversized frame the helper would refuse.
    let samples = vec![7i16; frame::MAX_SAMPLES_PER_FRAME * 3 + 5];
    big.send_audio(&samples).await.expect("audio");
    big.send_audio(&[]).await.expect("empty audio is a no-op");
    big.finish().await.expect("finish");
    assert_eq!(
        final_text(&mut big).await,
        format!("samples={}", samples.len())
    );
    client.shutdown().await;
}

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
use fotw_speechd_client::message::{
    CloseReason, ErrorCode, PROTOCOL_VERSION, ProgressPhase, Reply,
};
use fotw_speechd_client::{
    ClientError, Progress, RestartPolicy, SpeechStream, SpeechdClient, SpeechdConfig, StreamConfig,
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
    // A helper's own fatal error is followed at once by its exit: the client
    // must still read the error rather than report a plain crash. Repeated,
    // because which side notices first is a race.
    let cases = ["oversized", "not-json", "missing-field", "audio"]
        .into_iter()
        .chain(std::iter::repeat_n("fatal-error", 10));
    for misbehaviour in cases {
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
    let spawns = client.status().spawns;
    assert_eq!(spawns.len(), 2, "one restart: {spawns:?}");
    assert!(
        spawns[1] - spawns[0] >= RestartPolicy::default().min_interval,
        "spawns at least 2 s apart: {:?}",
        spawns[1] - spawns[0]
    );
    assert!(started.elapsed() < Duration::from_secs(10));
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
    config.heartbeat_timeout = Duration::from_millis(400);
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
async fn a_finished_stream_with_no_result_by_its_deadline_fails_instead_of_waiting_forever() {
    // The helper still answers health, so the heartbeat alone cannot catch a
    // wedged engine.
    let mut config = config(&[]);
    config.finish_timeout = Duration::from_millis(300);
    let client = SpeechdClient::start(config).await.expect("start");
    let first_pid = client.helper_pid().expect("a helper is running");
    client.load("fake-stall").await.expect("load");
    let mut stalled = client
        .open_stream(stream("fake-stall"))
        .await
        .expect("open");
    stalled.send_audio(&[1; 16]).await.expect("audio");
    stalled.finish().await.expect("finish");
    let waited = Instant::now();
    match next(&mut stalled).await {
        Some(StreamEvent::Failed(failure)) => {
            assert_eq!(failure.code, ErrorCode::HelperUnresponsive)
        }
        other => panic!("expected helper_unresponsive, got {other:?}"),
    }
    assert!(waited.elapsed() < Duration::from_secs(1));
    // The wedged helper was killed and a replacement started.
    let replaced = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.helper_pid() {
                Some(pid) if pid != first_pid => return pid,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    assert!(replaced.is_ok(), "no replacement helper");
    client.shutdown().await;
    assert_eq!(client.helper_pid(), None);
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
    let spawns = client.status().spawns;
    assert_eq!(spawns.len(), 4, "the first spawn and 3 restarts");
    for pair in spawns.windows(2) {
        assert!(pair[1] - pair[0] >= Duration::from_millis(50), "{spawns:?}");
    }
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

#[tokio::test]
async fn a_result_written_just_before_the_helper_dies_is_still_delivered() {
    // The helper writes final and closed, then exits at once. Which the client
    // notices first, the exit or the frames, is a race, so run it repeatedly.
    let mut config = config(&[]);
    config.restart.min_interval = Duration::from_millis(50);
    let client = SpeechdClient::start(config).await.expect("start");
    client.load("fake-exit-after-final").await.expect("load");
    for round in 0..10 {
        let mut last = client
            .open_stream(stream("fake-exit-after-final"))
            .await
            .unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        last.send_audio(&[5; 320]).await.expect("audio");
        last.finish().await.expect("finish");
        assert_eq!(final_text(&mut last).await, "samples=320", "round {round}");
        // The helper exits right after; wait for its replacement before the
        // next round rather than racing a request into the dying one.
        let spawned = client.status().spawns.len();
        tokio::time::timeout(Duration::from_secs(5), async {
            while client.status().spawns.len() == spawned || client.capabilities().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a replacement helper");
    }
    client.shutdown().await;
}

#[tokio::test]
async fn the_default_deadlines_find_a_helper_hung_after_finish_within_three_seconds() {
    // After finish a batch stream sends no audio, so only the heartbeat can
    // notice. Default config: a 1 s heartbeat with a 2 s deadline.
    let client = SpeechdClient::start(config(&[])).await.expect("start");
    client.load("fake-hang").await.expect("load");
    let mut stuck = client.open_stream(stream("fake-hang")).await.expect("open");
    stuck.send_audio(&[1; 16]).await.expect("audio");
    stuck.finish().await.expect("finish");
    let waited = Instant::now();
    match next(&mut stuck).await {
        Some(StreamEvent::Failed(failure)) => {
            assert_eq!(failure.code, ErrorCode::HelperUnresponsive);
        }
        other => panic!("expected helper_unresponsive, got {other:?}"),
    }
    assert!(
        waited.elapsed() < Duration::from_millis(3_500),
        "{:?}",
        waited.elapsed()
    );
    client.shutdown().await;
}

#[tokio::test]
async fn a_slow_finish_never_blocks_other_streams_audio_or_health() {
    // fake-batch takes over 2 s to finish, like a long batch on the real
    // engine. Tight deadlines: a helper that read or answered nothing in the
    // meantime would be killed.
    let mut config = config(&[]);
    config.audio_stall_timeout = Duration::from_millis(500);
    config.heartbeat_interval = Duration::from_millis(100);
    config.heartbeat_timeout = Duration::from_millis(500);
    let client = SpeechdClient::start(config).await.expect("start");
    client.load("fake-batch").await.expect("load");
    client.load("fake-stream").await.expect("load");
    let mut slow = client
        .open_stream(stream("fake-batch"))
        .await
        .expect("open");
    let mut live = client
        .open_stream(stream("fake-stream"))
        .await
        .expect("open");
    slow.send_audio(&[1; 1600]).await.expect("audio");
    slow.finish().await.expect("finish");
    let finished = Instant::now();
    for round in 1..=20u32 {
        live.send_audio(&[2; 160])
            .await
            .expect("audio while the other finishes");
        match next(&mut live).await {
            Some(StreamEvent::Partial { text, .. }) => {
                assert_eq!(text, format!("samples={}", 160 * round));
            }
            other => panic!("expected a partial, got {other:?}"),
        }
        client
            .health()
            .await
            .expect("health while the other finishes");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(final_text(&mut slow).await, "samples=1600");
    assert!(
        finished.elapsed() >= Duration::from_secs(2),
        "fake-batch must model a slow finish: {:?}",
        finished.elapsed()
    );
    client.shutdown().await;
}

#[tokio::test]
async fn unknown_messages_and_engine_kinds_from_a_newer_helper_are_ignored() {
    let Some(config) = fake_only(&["--from-the-future"]) else {
        return;
    };
    let client = SpeechdClient::start(config)
        .await
        .expect("start despite a new engine kind");
    let caps = client.capabilities().expect("capabilities");
    assert!(
        caps.engine("fake-hologram").is_none(),
        "unknown kinds are left out"
    );
    assert!(caps.engine("fake-batch").is_some());
    assert!(matches!(
        client.open_stream(stream("fake-hologram")).await,
        Err(ClientError::UnknownEngine(_))
    ));
    // Each health answer is preceded by an id-less message of an unknown type.
    for _ in 0..3 {
        client
            .health()
            .await
            .expect("unknown id-less messages are skipped");
    }
    client.shutdown().await;
}

#[tokio::test]
async fn streaming_partials_and_finals_carry_their_utterance() {
    let client = SpeechdClient::start(config(&[])).await.expect("start");
    client.load("fake-stream").await.expect("load");
    let mut live = client
        .open_stream(stream("fake-stream"))
        .await
        .expect("open");
    live.send_audio(&[1; 160]).await.expect("audio");
    match next(&mut live).await {
        Some(StreamEvent::Partial { utterance, .. }) => assert_eq!(utterance, Some(1)),
        other => panic!("expected a partial, got {other:?}"),
    }
    live.finish().await.expect("finish");
    match next(&mut live).await {
        Some(StreamEvent::Final(f)) => assert_eq!(f.utterance, Some(1)),
        other => panic!("expected a final, got {other:?}"),
    }
    client.shutdown().await;
}

#[tokio::test]
async fn load_and_warmup_report_progress() {
    let client = SpeechdClient::start(config(&[])).await.expect("start");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    client
        .load_with_progress("fake-batch", Some(tx.clone()))
        .await
        .expect("load");
    client
        .warmup_with_progress("fake-batch", Some(tx))
        .await
        .expect("warmup");
    let mut phases = Vec::new();
    while let Ok(progress) = rx.try_recv() {
        phases.push(progress.phase);
    }
    assert!(phases.contains(&ProgressPhase::Load), "{phases:?}");
    assert!(phases.contains(&ProgressPhase::Warmup), "{phases:?}");
    client.shutdown().await;
}

#[tokio::test]
async fn try_send_audio_reports_backpressure_instead_of_blocking() {
    let mut config = config(&[]);
    config.heartbeat_interval = Duration::from_secs(60);
    let client = SpeechdClient::start(config).await.expect("start");
    client.load("fake-hang").await.expect("load");
    let stuck = client.open_stream(stream("fake-hang")).await.expect("open");
    let started = Instant::now();
    let mut outcome = None;
    for _ in 0..10_000 {
        if let Err(e) = stuck.try_send_audio(&[1; 1000]) {
            outcome = Some(e);
            break;
        }
    }
    assert_eq!(outcome, Some(ClientError::Backpressure));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "never blocked"
    );
    client.shutdown().await;
}

/// The fake behind a relay that hands each chunk of its output on 50 ms after
/// it was written (a fixed latency, not one per chunk that could pile up), in
/// a wrapper process that exits as soon as the fake does: the client sees the
/// exit before it has read what the helper wrote just before dying.
fn behind_a_slow_relay(extra: &[&str]) -> Option<SpeechdConfig> {
    let (fake, true) = helper() else { return None };
    let script = r#"f="$(mktemp -u)"; mkfifo "$f"
python3 -u -c 'import os, queue, sys, threading, time
src, chunks = os.open(sys.argv[1], os.O_RDONLY), queue.Queue()
def read():
    while True:
        chunk = os.read(src, 65536)
        chunks.put((time.monotonic() + 0.05, chunk))
        if not chunk:
            return
threading.Thread(target=read, daemon=True).start()
while True:
    due, chunk = chunks.get()
    time.sleep(max(0.0, due - time.monotonic()))
    if not chunk:
        break
    try:
        os.write(1, chunk)
    except BrokenPipeError:
        break' "$f" &
"$0" "$@" > "$f"; status=$?; rm -f "$f"; exit $status"#;
    let mut config = SpeechdConfig::new("/bin/sh", std::env::temp_dir());
    let mut args: Vec<std::ffi::OsString> = vec!["-c".into(), script.into(), fake.into()];
    args.append(&mut config.args);
    args.push("--fake-engines".into());
    args.extend(extra.iter().map(Into::into));
    config.args = args;
    config.restart.min_interval = Duration::from_millis(50);
    Some(config)
}

#[tokio::test]
async fn frames_written_before_the_helper_exits_are_read_before_it_is_declared_dead() {
    let Some(config) = behind_a_slow_relay(&[]) else {
        return;
    };
    let client = SpeechdClient::start(config).await.expect("start");
    client.load("fake-exit-after-final").await.expect("load");
    let mut last = client
        .open_stream(stream("fake-exit-after-final"))
        .await
        .expect("open");
    last.send_audio(&[5; 320]).await.expect("audio");
    last.finish().await.expect("finish");
    assert_eq!(
        final_text(&mut last).await,
        "samples=320",
        "the final is not lost"
    );
    client.shutdown().await;

    // The helper's own fatal error, then its exit: reported as what it is.
    let config = behind_a_slow_relay(&["--misbehave-on-health", "fatal-error"]).unwrap();
    let client = SpeechdClient::start(config).await.expect("start");
    match client.health().await {
        Err(ClientError::Protocol(message)) => {
            assert!(message.contains("told to fail"), "{message}")
        }
        other => panic!("expected the helper's protocol error, got {other:?}"),
    }
    client.shutdown().await;
}

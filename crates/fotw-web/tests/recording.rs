//! `GET /api/recording/status`, `POST /api/recording/{start,stop}`.
//!
//! The UI's Start button. Three properties matter more than the happy path:
//!
//! 1. **The state never rides on the status code.** "Nothing is recording" is
//!    `200 {"state":"idle"}`, not a 404 or a 409. A status code that varied
//!    with recording state would answer "is this person in a meeting right
//!    now" to anyone who could reach the port — which is the single fact
//!    ING-09 exists to withhold.
//! 2. **Starting requires an explicit acknowledgement** (CON-01). The CLI
//!    spells it `--i-have-consent`; the UI spells it a tick box. Neither may
//!    be skippable, and a Start that silently records is the failure mode the
//!    requirement is written against.
//! 3. **A daemon with no recorder is indistinguishable from one with no such
//!    route.** The read-only preview server exists, and it must not advertise
//!    a capability it does not have.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use fotw_web::{
    MemorySource, RecorderControl, RecorderError, RecordingState, RecordingStatus, WebServer,
};

/// When the fake's meeting began, and when Stop froze it.
const FAKE_STARTED_MS: u64 = 1_787_000_000_000;
const FAKE_ENDED_MS: u64 = FAKE_STARTED_MS + 90_000;

/// The slot `DaemonRecorder` holds, without a device in it.
#[derive(Debug, Clone, Copy)]
struct FakeLive {
    started_at_ms: u64,
    /// `Some` once Stop has been pressed: capture is over, the meeting is not
    /// yet on disk. Set once, so a reloaded tab's second Stop cannot shift the
    /// frozen clock — the same rule `DaemonRecorder::stop` follows.
    stopped_at_ms: Option<u64>,
}

/// A recorder that walks the same three states the daemon does, without
/// touching a device.
///
/// The two doubles used to encode opposite contracts — this one returned
/// `idle()` from `stop()` while `DaemonRecorder` returned `recording` — and
/// nothing bridged them, which is why #77 looked fine in CI for the life of
/// the feature. `finish()` is the test's hand on the slot clear at the end of
/// `fotwd`'s session task: nothing here happens on a timer.
#[derive(Debug, Default)]
struct FakeRecorder {
    live: Mutex<Option<FakeLive>>,
    starts: AtomicU64,
    stops: AtomicU64,
}

impl FakeRecorder {
    /// The meeting reached the library: the slot frees and Start works again.
    ///
    /// Test-only, and deliberately not on [`RecorderControl`] — the real
    /// daemon reaches this state on its own, from the session task.
    fn finish(&self) {
        *self.live.lock().unwrap() = None;
    }
}

impl RecorderControl for FakeRecorder {
    fn start(&self) -> Result<RecordingStatus, RecorderError> {
        let mut live = self.live.lock().unwrap();
        // Occupied covers finishing as well as recording: the tap is closed
        // but the session still owns the device and the session directory.
        if live.is_some() {
            return Err(RecorderError::AlreadyRecording);
        }
        self.starts.fetch_add(1, Ordering::Relaxed);
        *live = Some(FakeLive {
            started_at_ms: FAKE_STARTED_MS,
            stopped_at_ms: None,
        });
        Ok(RecordingStatus::recording(FAKE_STARTED_MS, 0))
    }

    fn stop(&self) -> Result<RecordingStatus, RecorderError> {
        let mut live = self.live.lock().unwrap();
        let Some(live) = live.as_mut() else {
            return Err(RecorderError::NotRecording);
        };
        self.stops.fetch_add(1, Ordering::Relaxed);
        let ended = *live.stopped_at_ms.get_or_insert(FAKE_ENDED_MS);
        Ok(RecordingStatus::finishing(live.started_at_ms, ended))
    }

    fn status(&self) -> RecordingStatus {
        match *self.live.lock().unwrap() {
            None => RecordingStatus::idle(),
            Some(FakeLive {
                started_at_ms,
                stopped_at_ms: Some(ended),
            }) => RecordingStatus::finishing(started_at_ms, ended),
            Some(FakeLive { started_at_ms, .. }) => {
                RecordingStatus::recording(started_at_ms, 1_000)
            }
        }
    }
}

struct Rig {
    h: common::Harness,
    recorder: Arc<FakeRecorder>,
}

async fn rig() -> Rig {
    let recorder = Arc::new(FakeRecorder::default());
    let server = WebServer::bind_with_recorder(
        0,
        Arc::new(MemorySource::new()),
        Some(Arc::clone(&recorder) as Arc<dyn RecorderControl>),
    )
    .await
    .expect("bind loopback");

    let addr = server.addr();
    let state = server.state().clone();
    let h = common::Harness {
        addr,
        token: state.policy().secret().expose_hex(),
        authority: state.policy().authority().to_owned(),
        origin: state.policy().origin().to_owned(),
        state,
    };
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    Rig { h, recorder }
}

fn state_of(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).expect("json body");
    v["state"].as_str().unwrap_or_default().to_owned()
}

// ------------------------------------------------------------------ ING-05

/// All three need the bearer. They are not on `bearer_exempt`'s allowlist, and
/// this is the test that fails if someone adds them.
#[tokio::test]
async fn every_recording_route_needs_the_bearer() {
    let r = rig().await;
    let anon = r.h.anonymous();

    for (method, path) in [
        ("GET", "/api/recording/status"),
        ("POST", "/api/recording/start"),
        ("POST", "/api/recording/stop"),
    ] {
        let res = match method {
            "GET" => r.h.get(path, &anon).await,
            _ => r.h.post(path, &anon, None).await,
        };
        assert_eq!(
            res.status, 404,
            "{method} {path} leaked to an anonymous caller"
        );
    }

    assert_eq!(
        r.recorder.starts.load(Ordering::Relaxed),
        0,
        "an unauthenticated request reached the recorder"
    );
}

/// ING-09: a refused credential and an unknown path must be byte-identical, so
/// a scanning page cannot learn that the recording API exists.
#[tokio::test]
async fn a_refused_start_is_byte_identical_to_an_unknown_path() {
    let r = rig().await;
    let anon = r.h.anonymous();

    let refused = r.h.post("/api/recording/start", &anon, None).await;
    let unknown = r.h.post("/no-such-path", &anon, None).await;

    assert_eq!(
        refused.bytes_without_date(),
        unknown.bytes_without_date(),
        "the recording API is distinguishable from an unknown path"
    );
}

/// A wrong method must not answer with a 405 carrying `Allow`, which would
/// confirm the path exists.
#[tokio::test]
async fn a_wrong_method_does_not_advertise_the_route() {
    let r = rig().await;
    let res = r.h.get("/api/recording/start", &r.h.authorised()).await;

    assert_eq!(res.status, 404);
    assert!(
        res.header("allow").is_none(),
        "the Allow header confirms the path exists"
    );
}

// ------------------------------------------------------------------- state

#[tokio::test]
async fn a_daemon_that_has_recorded_nothing_reports_idle() {
    let r = rig().await;
    let res = r.h.get("/api/recording/status", &r.h.authorised()).await;

    assert_eq!(res.status, 200, "idle is a 200, never a 404");
    assert_eq!(state_of(&res.body), "idle");
}

#[tokio::test]
async fn an_acknowledged_start_records_and_status_agrees() {
    let r = rig().await;
    let auth = r.h.authorised();

    let started =
        r.h.post("/api/recording/start?ack=all-party", &auth, None)
            .await;
    assert_eq!(started.status, 200);
    assert_eq!(state_of(&started.body), "recording");

    let status = r.h.get("/api/recording/status", &auth).await;
    assert_eq!(state_of(&status.body), "recording");

    let v: serde_json::Value = serde_json::from_str(&status.body).unwrap();
    assert!(
        v["started_at_ms"].as_u64().is_some(),
        "a live recording must say when it began: {}",
        status.body
    );
}

/// CON-01. The tick box is the whole control; a Start that works without it is
/// a Start that records silently.
#[tokio::test]
async fn a_start_without_the_acknowledgement_is_refused() {
    let r = rig().await;
    let res =
        r.h.post("/api/recording/start", &r.h.authorised(), None)
            .await;

    assert_eq!(res.status, 200, "the refusal is data, not a status code");
    assert_eq!(state_of(&res.body), "idle");
    assert_eq!(
        r.recorder.starts.load(Ordering::Relaxed),
        0,
        "the recorder was started without an acknowledgement"
    );

    let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(v["error"].as_str(), Some("consent_required"));
}

/// A wrong value is not an acknowledgement either — otherwise `?ack=` or
/// `?ack=no` would start a recording.
#[tokio::test]
async fn a_wrong_acknowledgement_value_is_refused() {
    let r = rig().await;
    let auth = r.h.authorised();

    for q in ["?ack=", "?ack=no", "?ack=true", "?acknowledged=all-party"] {
        let res =
            r.h.post(&format!("/api/recording/start{q}"), &auth, None)
                .await;
        assert_eq!(state_of(&res.body), "idle", "{q} was accepted");
    }
    assert_eq!(r.recorder.starts.load(Ordering::Relaxed), 0);
}

/// Stop is not the end of the session (#77). The daemon holds its slot until
/// the meeting is genuinely on disk, and the honest word for that window is
/// `finishing` — this double used to answer `idle` and the daemon answered
/// `recording`, so the dashboard's clock kept climbing past Stop.
#[tokio::test]
async fn stopping_reports_finishing_until_the_meeting_is_on_disk() {
    let r = rig().await;
    let auth = r.h.authorised();

    r.h.post("/api/recording/start?ack=all-party", &auth, None)
        .await;
    let stopped = r.h.post("/api/recording/stop", &auth, None).await;

    assert_eq!(stopped.status, 200);
    assert_eq!(state_of(&stopped.body), "finishing");
    assert_eq!(r.recorder.stops.load(Ordering::Relaxed), 1);

    let v: serde_json::Value = serde_json::from_str(&stopped.body).unwrap();
    assert_eq!(v["started_at_ms"].as_u64(), Some(FAKE_STARTED_MS));
    assert_eq!(v["ended_at_ms"].as_u64(), Some(FAKE_ENDED_MS));
    assert_eq!(
        v["elapsed_ms"].as_u64(),
        Some(FAKE_ENDED_MS - FAKE_STARTED_MS),
        "the finishing clock is frozen at the meeting's final length: {}",
        stopped.body
    );

    // The status route agrees with the stop response until the slot clears —
    // a tab reloaded mid-finalize must see the same thing.
    assert_eq!(
        state_of(&r.h.get("/api/recording/status", &auth).await.body),
        "finishing"
    );

    // Only the meeting landing ends it, which is what `finish()` models.
    r.recorder.finish();
    assert_eq!(
        state_of(&r.h.get("/api/recording/status", &auth).await.body),
        "idle"
    );
}

/// A second Stop is what a reloaded tab does. It must not move the frozen
/// clock forward, or the meeting appears to grow after it ended.
#[tokio::test]
async fn a_second_stop_keeps_the_first_end_time() {
    let r = rig().await;
    let auth = r.h.authorised();

    r.h.post("/api/recording/start?ack=all-party", &auth, None)
        .await;
    let first = r.h.post("/api/recording/stop", &auth, None).await;
    let second = r.h.post("/api/recording/stop", &auth, None).await;

    assert_eq!(state_of(&second.body), "finishing");
    let a: serde_json::Value = serde_json::from_str(&first.body).unwrap();
    let b: serde_json::Value = serde_json::from_str(&second.body).unwrap();
    assert_eq!(a["ended_at_ms"], b["ended_at_ms"]);
    assert_eq!(a["elapsed_ms"], b["elapsed_ms"]);
}

/// A Start arriving while the previous meeting is still being written is
/// refused — the slot is occupied — and the body beside the refusal must say
/// `finishing`, because that is what the dashboard renders. ING-09: the
/// refusal is data, so the status code does not move.
#[tokio::test]
async fn starting_during_finishing_is_refused_and_says_finishing() {
    let r = rig().await;
    let auth = r.h.authorised();

    r.h.post("/api/recording/start?ack=all-party", &auth, None)
        .await;
    r.h.post("/api/recording/stop", &auth, None).await;

    let again =
        r.h.post("/api/recording/start?ack=all-party", &auth, None)
            .await;

    assert_eq!(again.status, 200);
    assert_eq!(state_of(&again.body), "finishing");
    let v: serde_json::Value = serde_json::from_str(&again.body).unwrap();
    assert_eq!(v["error"].as_str(), Some("already_recording"));
    assert_eq!(
        r.recorder.starts.load(Ordering::Relaxed),
        1,
        "a second tap was opened while the first meeting was still being written"
    );
}

/// Double-clicking Start must not open a second tap on the same device.
#[tokio::test]
async fn starting_twice_does_not_start_twice() {
    let r = rig().await;
    let auth = r.h.authorised();

    r.h.post("/api/recording/start?ack=all-party", &auth, None)
        .await;
    let again =
        r.h.post("/api/recording/start?ack=all-party", &auth, None)
            .await;

    assert_eq!(again.status, 200);
    assert_eq!(
        r.recorder.starts.load(Ordering::Relaxed),
        1,
        "the second Start opened another recording"
    );
    assert_eq!(
        state_of(&again.body),
        "recording",
        "the answer is still the truth about the state"
    );
}

#[tokio::test]
async fn stopping_when_idle_is_not_an_error_the_ui_must_handle() {
    let r = rig().await;
    let res =
        r.h.post("/api/recording/stop", &r.h.authorised(), None)
            .await;

    assert_eq!(res.status, 200);
    assert_eq!(state_of(&res.body), "idle");
}

// ------------------------------------------------------- no recorder wired

/// `fotwd serve` on a build with no capture, and the `ui_preview` example,
/// both run with `None`. They must look exactly like a server that has never
/// heard of these paths.
#[tokio::test]
async fn a_server_without_a_recorder_hides_the_routes_entirely() {
    let h = common::start().await;
    let auth = h.authorised();

    for (method, path) in [
        ("GET", "/api/recording/status"),
        ("POST", "/api/recording/start?ack=all-party"),
        ("POST", "/api/recording/stop"),
    ] {
        let res = match method {
            "GET" => h.get(path, &auth).await,
            _ => h.post(path, &auth, None).await,
        };
        assert_eq!(
            res.status, 404,
            "{method} {path} answered without a recorder"
        );

        let unknown = h.get("/no-such-path", &auth).await;
        assert_eq!(
            res.bytes_without_date().len(),
            unknown.bytes_without_date().len(),
            "the absent recorder is distinguishable from an unknown path"
        );
    }
}

/// ING-08 holds for the new responses too.
#[tokio::test]
async fn no_recording_response_sets_a_cookie() {
    let r = rig().await;
    let auth = r.h.authorised();

    let responses = [
        r.h.get("/api/recording/status", &auth).await,
        r.h.post("/api/recording/start?ack=all-party", &auth, None)
            .await,
        r.h.post("/api/recording/stop", &auth, None).await,
    ];
    for res in responses {
        assert!(res.header("set-cookie").is_none());
    }
}

/// The state enum is what the UI switches on, so its wire spelling is part of
/// the contract rather than an implementation detail.
#[test]
fn the_wire_spelling_of_the_states_is_stable() {
    assert_eq!(
        serde_json::to_value(RecordingState::Idle).unwrap(),
        serde_json::Value::String("idle".into())
    );
    assert_eq!(
        serde_json::to_value(RecordingState::Recording).unwrap(),
        serde_json::Value::String("recording".into())
    );
    assert_eq!(
        serde_json::to_value(RecordingState::Finishing).unwrap(),
        serde_json::Value::String("finishing".into())
    );
}

/// An idle status must not carry a start time — the UI renders "since HH:MM"
/// from it, and `null` is the only value that cannot be rendered by accident.
#[test]
fn an_idle_status_carries_no_timestamps() {
    let v = serde_json::to_value(RecordingStatus::idle()).unwrap();
    assert_eq!(v["state"], "idle");
    assert!(v["started_at_ms"].is_null());
    assert!(v["elapsed_ms"].is_null());
    // Present and null, not absent: finishing (#77) carries an end time, and a
    // key that vanishes when idle is one the UI has to guard rather than read.
    // `v["ended_at_ms"].is_null()` would pass for a field never added at all.
    assert_eq!(
        v.get("ended_at_ms"),
        Some(&serde_json::Value::Null),
        "idle must spell the end time null rather than omit it: {v}"
    );
}

/// The finishing body is the whole of #77: a word the UI can switch on and a
/// clock that has stopped. `elapsed_ms` is frozen at the meeting's length
/// rather than counting on, because capture ended and the session cannot get
/// any longer.
#[test]
fn a_finishing_status_carries_a_frozen_clock() {
    let v = serde_json::to_value(RecordingStatus::finishing(
        FAKE_STARTED_MS,
        FAKE_STARTED_MS + 90_000,
    ))
    .unwrap();
    assert_eq!(v["state"], "finishing");
    assert_eq!(v["started_at_ms"].as_u64(), Some(FAKE_STARTED_MS));
    assert_eq!(v["ended_at_ms"].as_u64(), Some(FAKE_STARTED_MS + 90_000));
    assert_eq!(v["elapsed_ms"].as_u64(), Some(90_000));
}

/// A tab loaded mid-finalize must still see a provider failure. The field
/// exists so an empty transcript is never mistaken for a quiet meeting, and
/// the end of the meeting is exactly when someone goes looking for the words.
#[test]
fn a_finishing_status_can_still_report_a_transcription_failure() {
    let v = serde_json::to_value(
        RecordingStatus::finishing(FAKE_STARTED_MS, FAKE_ENDED_MS)
            .with_transcription_error(Some("deepgram closed the socket".to_owned())),
    )
    .unwrap();
    assert_eq!(v["state"], "finishing");
    assert_eq!(
        v["transcription_error"].as_str(),
        Some("deepgram closed the socket")
    );
}

// ------------------------------------------------- transcription failures

/// A recording whose transcription provider is failing must say so *while it
/// is running*. Discovering it afterwards is how two fatal Deepgram bugs
/// survived the life of the project: an empty transcript beside good audio is
/// indistinguishable from a meeting where nobody spoke.
#[tokio::test]
async fn a_transcription_failure_is_visible_in_the_status() {
    let status = RecordingStatus::recording(1_787_000_000_000, 5_000)
        .with_transcription_error(Some("deepgram handshake returned HTTP 400".to_owned()));

    let v = serde_json::to_value(&status).unwrap();
    assert_eq!(v["state"], "recording");
    assert_eq!(
        v["transcription_error"].as_str(),
        Some("deepgram handshake returned HTTP 400")
    );
}

/// Silence in this field means "nothing has gone wrong", so it must be null
/// rather than an empty string the UI would render as a blank warning.
#[tokio::test]
async fn a_healthy_recording_reports_no_transcription_error() {
    let v = serde_json::to_value(RecordingStatus::recording(1, 2)).unwrap();
    assert!(v["transcription_error"].is_null());

    let idle = serde_json::to_value(RecordingStatus::idle()).unwrap();
    assert!(idle["transcription_error"].is_null());
}

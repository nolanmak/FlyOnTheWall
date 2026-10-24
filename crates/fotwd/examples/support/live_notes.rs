//! Synthetic recorder for exercising the real live-note API without audio.
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fotw_store::{Db, DbKey, NewMeeting};
use fotw_web::recorder::{RecorderControl, RecorderError, RecordingStatus};

struct State {
    db: Db,
    started_at_ms: u64,
    started: Instant,
    stopped: Option<(u64, Instant)>,
}
pub(super) struct FixtureRecorder {
    session: String,
    state: Mutex<State>,
    hub: OnceLock<Arc<fotw_web::DeltaHub>>,
}
impl FixtureRecorder {
    pub(super) fn attach_hub(&self, hub: Arc<fotw_web::DeltaHub>) {
        let _ = self.hub.set(hub);
    }

    fn status_for(&self, state: &State) -> RecordingStatus {
        match state.stopped {
            Some((_, at)) if at.elapsed() >= Duration::from_secs(1) => RecordingStatus::idle(),
            Some((end, _)) => RecordingStatus::finishing(state.started_at_ms, end)
                .with_session_id(Some(self.session.clone())),
            None => RecordingStatus::recording(
                state.started_at_ms,
                u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            )
            .with_session_id(Some(self.session.clone())),
        }
    }
}
impl RecorderControl for FixtureRecorder {
    fn start(&self) -> Result<RecordingStatus, RecorderError> {
        if self.status().is_active() {
            Err(RecorderError::AlreadyRecording)
        } else {
            Err(RecorderError::Failed(
                "Synthetic fixture finished; rerun ui_preview --live-notes to reset".into(),
            ))
        }
    }
    fn stop(&self) -> Result<RecordingStatus, RecorderError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.stopped.is_none() {
            let elapsed = u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let end = state.started_at_ms.saturating_add(elapsed);
            let mut meeting = NewMeeting::new("synthetic-preview", "UTC");
            meeting.title = "Synthetic live notes".into();
            meeting.started_at_ms = i64::try_from(state.started_at_ms)
                .map_err(|error| RecorderError::Failed(error.to_string()))?;
            let meeting_id = state
                .db
                .finalize_capture(
                    &self.session,
                    meeting,
                    i64::try_from(end).map_err(|error| RecorderError::Failed(error.to_string()))?,
                    None,
                )
                .map_err(|error| RecorderError::Failed(error.to_string()))?;
            state.stopped = Some((end, Instant::now()));
            if let Some(hub) = self.hub.get() {
                hub.announce_session_ready(
                    &meeting_id,
                    fotw_web::MeetingReadyReason::Persisted,
                    Some(&self.session),
                );
            }
        }
        Ok(self.status_for(&state))
    }
    fn status(&self) -> RecordingStatus {
        self.status_for(&self.state.lock().unwrap_or_else(|error| error.into_inner()))
    }
}

/// A one-shot synthetic recording with the real encrypted note finalization path.
pub(super) fn recorder(root: &Path) -> Result<Arc<FixtureRecorder>, String> {
    let db = Db::open(
        root.join("db.sqlite3"),
        &DbKey::from_bytes(super::PREVIEW_KEY),
    )
    .map_err(|error| error.to_string())?;
    let started_at_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis(),
    )
    .map_err(|error| error.to_string())?;
    Ok(Arc::new(FixtureRecorder {
        session: format!("preview-live-{}", std::process::id()),
        hub: OnceLock::new(),
        state: Mutex::new(State {
            db,
            started_at_ms,
            started: Instant::now(),
            stopped: None,
        }),
    }))
}

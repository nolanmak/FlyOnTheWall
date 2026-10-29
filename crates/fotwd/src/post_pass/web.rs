//! The dashboard's view of the cleanup: progress, "Show live transcript",
//! Retry and Cancel, and the setting.

use std::sync::Mutex;

use fotw_store::{Db, PostPassState};
use fotw_web::cleanup::{
    CleanupControl, CleanupMode as WebMode, CleanupPhase, CleanupSettingsDoc, CleanupStatus,
    TranscriptVersion,
};

use super::{CleanupMode, CleanupSettings};

/// What the dashboard can ask of the cleanup worker.
pub trait JobActions: Send + Sync {
    /// Queue a failed job again; whether it was failed.
    fn retry(&self, meeting: &str) -> bool;
    /// Cancel a pending job; whether one was pending.
    fn cancel(&self, meeting: &str) -> bool;
}

impl JobActions for super::controller::Controller {
    fn retry(&self, meeting: &str) -> bool {
        super::controller::Controller::retry(self, meeting)
    }
    fn cancel(&self, meeting: &str) -> bool {
        super::controller::Controller::cancel(self, meeting)
    }
}

/// [`CleanupControl`] over the daemon's library.
pub struct DashboardCleanup {
    db: Mutex<Db>,
    actions: std::sync::Arc<dyn JobActions>,
}

impl std::fmt::Debug for DashboardCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DashboardCleanup(<redacted>)")
    }
}

impl DashboardCleanup {
    /// Over its own library connection.
    #[must_use]
    pub fn new(db: Db, actions: std::sync::Arc<dyn JobActions>) -> Self {
        Self {
            db: Mutex::new(db),
            actions,
        }
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn read_status(db: &mut Db, meeting: &str) -> Option<CleanupStatus> {
    db.meetings().get(meeting).ok()?;
    let Some(job) = db.post_pass_job(meeting).ok()? else {
        return Some(CleanupStatus {
            phase: CleanupPhase::None,
            percent: 0,
            showing: None,
            detail: None,
        });
    };
    let primary = db.meetings().primary_transcript_id(meeting).ok()?;
    let showing = match primary {
        Some(id) if job.cleaned_transcript_id.as_ref() == Some(&id) => {
            Some(TranscriptVersion::Cleaned)
        }
        Some(id) if job.live_transcript_id.as_ref() == Some(&id) => Some(TranscriptVersion::Live),
        _ => None,
    };
    Some(CleanupStatus {
        phase: match job.state {
            PostPassState::Pending => CleanupPhase::Pending,
            PostPassState::Done => CleanupPhase::Done,
            PostPassState::Failed => CleanupPhase::Failed,
        },
        percent: (job.progress_permille / 10).clamp(0, 100) as u8,
        showing,
        detail: job.detail,
    })
}

impl CleanupControl for DashboardCleanup {
    fn status(&self, meeting: &str) -> Option<CleanupStatus> {
        read_status(&mut self.db(), meeting)
    }

    fn show(&self, meeting: &str, version: TranscriptVersion) -> Option<CleanupStatus> {
        let mut db = self.db();
        let job = db.post_pass_job(meeting).ok()??;
        let target = match version {
            TranscriptVersion::Live => job.live_transcript_id,
            TranscriptVersion::Cleaned => job.cleaned_transcript_id,
        }?;
        db.meetings().set_primary_transcript(&target).ok()?;
        read_status(&mut db, meeting)
    }

    fn settings(&self) -> CleanupSettingsDoc {
        CleanupSettingsDoc {
            mode: match CleanupSettings::read(&self.db()).mode {
                CleanupMode::Auto => WebMode::Auto,
                CleanupMode::On => WebMode::On,
                CleanupMode::Off => WebMode::Off,
            },
        }
    }

    fn retry(&self, meeting: &str) -> Option<CleanupStatus> {
        // The worker owns the job: it reopens the library itself, so this
        // connection's lock is not held across it.
        if !self.actions.retry(meeting) {
            return None;
        }
        read_status(&mut self.db(), meeting)
    }

    fn cancel(&self, meeting: &str) -> Option<CleanupStatus> {
        if !self.actions.cancel(meeting) {
            return None;
        }
        read_status(&mut self.db(), meeting)
    }

    fn set_settings(&self, doc: CleanupSettingsDoc) -> Result<(), String> {
        CleanupSettings {
            mode: match doc.mode {
                WebMode::Auto => CleanupMode::Auto,
                WebMode::On => CleanupMode::On,
                WebMode::Off => CleanupMode::Off,
            },
        }
        .write(&mut self.db())
    }
}

#[cfg(test)]
mod tests {
    use fotw_store::{DbKey, NewMeeting, NewSegment};

    use super::*;

    struct NoActions;
    impl JobActions for NoActions {
        fn retry(&self, _: &str) -> bool {
            false
        }
        fn cancel(&self, _: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn the_meeting_view_can_cancel_a_pending_cleanup_and_retry_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite3");
        let key = [0x54; 32];
        let open: super::super::controller::OpenLibrary = {
            let path = path.clone();
            std::sync::Arc::new(move || {
                Db::open(&path, &DbKey::from_bytes(key)).map_err(|e| e.to_string())
            })
        };
        let mut db = open().unwrap();
        let meeting = db
            .meetings()
            .create(NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        let live = db
            .meetings()
            .create_transcript(&meeting, "fotw-speechd", "parakeet-unified", true)
            .unwrap();
        db.post_pass_queue(&meeting, "{}").unwrap();
        // A worker that never runs anything: only the actions are under test.
        let controller = super::super::controller::Controller::new(
            dir.path(),
            open.clone(),
            std::sync::Arc::new(|| Box::pin(std::future::pending())),
            super::super::job::Plan::new(serde_json::json!({}), None),
            Box::new(|_| {}),
        );
        let control = DashboardCleanup::new(open().unwrap(), controller);

        let cancelled = control.cancel(&meeting).expect("a pending cleanup");
        assert_eq!(cancelled.phase, CleanupPhase::Failed);
        assert_eq!(cancelled.showing, Some(TranscriptVersion::Live));
        assert_eq!(
            cancelled.detail.as_deref(),
            Some(fotw_store::POST_PASS_CANCELLED)
        );
        assert!(control.cancel(&meeting).is_none(), "nothing left to cancel");

        let retried = control.retry(&meeting).expect("a failed cleanup");
        assert_eq!(retried.phase, CleanupPhase::Pending);
        assert!(control.retry(&meeting).is_none(), "already pending");
        assert_eq!(
            open()
                .unwrap()
                .meetings()
                .primary_transcript_id(&meeting)
                .unwrap(),
            Some(live)
        );
    }

    #[test]
    fn show_live_transcript_switches_back_and_forth_between_the_two_versions() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x52; 32])).unwrap();
        let meeting = db
            .meetings()
            .create(NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        let live = db
            .meetings()
            .create_transcript(&meeting, "fotw-speechd", "parakeet-unified", true)
            .unwrap();
        db.meetings()
            .append_segments(&live, &[NewSegment::new(0, 0, 900, "live words")])
            .unwrap();
        let control = DashboardCleanup::new(db, std::sync::Arc::new(NoActions));
        assert_eq!(control.status(&meeting).unwrap().phase, CleanupPhase::None);
        assert!(control.status("no-such-meeting").is_none());

        control.db().post_pass_queue(&meeting, "{}").unwrap();
        control
            .db()
            .post_pass_checkpoint(&meeting, "mic", 0, "[]", 400)
            .unwrap();
        let pending = control.status(&meeting).unwrap();
        assert_eq!(
            (pending.phase, pending.percent),
            (CleanupPhase::Pending, 40)
        );
        assert_eq!(pending.showing, Some(TranscriptVersion::Live));
        assert!(
            control.show(&meeting, TranscriptVersion::Cleaned).is_none(),
            "not there yet"
        );

        let cleaned = control
            .db()
            .post_pass_complete(
                &meeting,
                "fotw-speechd",
                "parakeet-unified",
                &[NewSegment::new(0, 0, 900, "clean")],
                "{}",
                None,
            )
            .unwrap();
        assert_eq!(
            control.status(&meeting).unwrap().showing,
            Some(TranscriptVersion::Cleaned)
        );

        let shown = control.show(&meeting, TranscriptVersion::Live).unwrap();
        assert_eq!(shown.showing, Some(TranscriptVersion::Live));
        let primary = control
            .db()
            .meetings()
            .primary_transcript_id(&meeting)
            .unwrap()
            .unwrap();
        assert_eq!(primary, live);
        assert_eq!(
            control.db().meetings().transcript_text(&primary).unwrap(),
            [(0, "live words".into())]
        );

        control.show(&meeting, TranscriptVersion::Cleaned).unwrap();
        assert_eq!(
            control
                .db()
                .meetings()
                .primary_transcript_id(&meeting)
                .unwrap(),
            Some(cleaned)
        );
    }

    #[test]
    fn the_setting_round_trips() {
        let control = DashboardCleanup::new(
            Db::open_in_memory(&DbKey::from_bytes([0x53; 32])).unwrap(),
            std::sync::Arc::new(NoActions),
        );
        assert_eq!(control.settings().mode, WebMode::Auto);
        control
            .set_settings(CleanupSettingsDoc { mode: WebMode::On })
            .unwrap();
        assert_eq!(control.settings().mode, WebMode::On);
    }
}

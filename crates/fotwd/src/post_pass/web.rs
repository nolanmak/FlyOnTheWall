//! The dashboard's view of the cleanup: progress, "Show live transcript",
//! and the setting.

use std::sync::Mutex;

use fotw_store::{Db, PostPassState};
use fotw_web::cleanup::{
    CleanupControl, CleanupMode as WebMode, CleanupPhase, CleanupSettingsDoc, CleanupStatus,
    TranscriptVersion,
};

use super::{CleanupMode, CleanupSettings};

/// [`CleanupControl`] over the daemon's library.
pub struct DashboardCleanup {
    db: Mutex<Db>,
}

impl std::fmt::Debug for DashboardCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DashboardCleanup(<redacted>)")
    }
}

impl DashboardCleanup {
    /// Over its own library connection.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db: Mutex::new(db) }
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
        let control = DashboardCleanup::new(db);
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
        let control =
            DashboardCleanup::new(Db::open_in_memory(&DbKey::from_bytes([0x53; 32])).unwrap());
        assert_eq!(control.settings().mode, WebMode::Auto);
        control
            .set_settings(CleanupSettingsDoc { mode: WebMode::On })
            .unwrap();
        assert_eq!(control.settings().mode, WebMode::On);
    }
}

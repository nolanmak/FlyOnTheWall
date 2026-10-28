//! Post-meeting cleanup jobs and their checkpoints (#246).
//!
//! A job re-transcribes a meeting's saved audio into a **new** transcript
//! version. The live transcript is never rewritten: the cleaned one is
//! created beside it and made primary in the same transaction that marks the
//! job done, and [`crate::MeetingRepo::set_primary_transcript`] switches back.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::error::{Result, StoreError};
use crate::ids::now_ms;
use crate::models::NewSegment;
use crate::repo::{insert_segments, insert_transcript};

/// How long summaries and the GitHub push wait for a pending cleanup before
/// they use the live transcript: 15 minutes.
pub const POST_PASS_WAIT_MS: i64 = 15 * 60 * 1_000;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostPassState {
    /// Queued or running.
    Pending,
    /// The cleaned version exists.
    Done,
    /// Stopped; `detail` says why. The live version stays primary.
    Failed,
}

impl PostPassState {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(StoreError::InvalidArgument(format!("unknown post-pass state {other}"))),
        }
    }

    /// As stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// One meeting's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostPassJob {
    /// The meeting.
    pub meeting_id: String,
    /// Where it is.
    pub state: PostPassState,
    /// When it was queued.
    pub created_at_ms: i64,
    /// Last change.
    pub updated_at_ms: i64,
    /// What it reads and how (JSON).
    pub plan_json: String,
    /// Progress, 0–1000.
    pub progress_permille: i64,
    /// The transcript that was primary when the job was queued.
    pub live_transcript_id: Option<String>,
    /// The version it produced.
    pub cleaned_transcript_id: Option<String>,
    /// Engines and revisions (JSON), once done.
    pub provenance_json: Option<String>,
    /// Why it failed, or what it left out.
    pub detail: Option<String>,
}

const COLUMNS: &str = "meeting_id, state, created_at_ms, updated_at_ms, plan_json, progress_permille,
     live_transcript_id, cleaned_transcript_id, provenance_json, detail";

fn read_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<(PostPassJob, String)> {
    Ok((
        PostPassJob {
            meeting_id: r.get(0)?,
            state: PostPassState::Pending,
            created_at_ms: r.get(2)?,
            updated_at_ms: r.get(3)?,
            plan_json: r.get(4)?,
            progress_permille: r.get(5)?,
            live_transcript_id: r.get(6)?,
            cleaned_transcript_id: r.get(7)?,
            provenance_json: r.get(8)?,
            detail: r.get(9)?,
        },
        r.get(1)?,
    ))
}

impl Db {
    /// Queue a job for `meeting_id`, remembering its current primary
    /// transcript as the live version. Returns `false` when it already has one.
    ///
    /// # Errors
    ///
    /// SQLite failures, or a meeting that does not exist.
    pub fn post_pass_queue(&mut self, meeting_id: &str, plan_json: &str) -> Result<bool> {
        let now = now_ms();
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO post_pass_jobs
               (meeting_id, state, created_at_ms, updated_at_ms, plan_json, live_transcript_id)
             VALUES (?1, 'pending', ?2, ?2, ?3,
                     (SELECT id FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1))",
            params![meeting_id, now, plan_json],
        )?;
        Ok(inserted == 1)
    }

    /// A meeting's job, if it has one.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn post_pass_job(&self, meeting_id: &str) -> Result<Option<PostPassJob>> {
        let row = self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM post_pass_jobs WHERE meeting_id = ?1"),
                params![meeting_id],
                read_job,
            )
            .optional()?;
        row.map(|(mut job, state)| {
            job.state = PostPassState::parse(&state)?;
            Ok(job)
        })
        .transpose()
    }

    /// Meetings with a pending job, oldest first: what a restarted daemon
    /// resumes.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn post_pass_pending(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT meeting_id FROM post_pass_jobs WHERE state = 'pending'
              ORDER BY created_at_ms, meeting_id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Checkpointed windows of one channel, as `(index, words JSON)`.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn post_pass_windows(&self, meeting_id: &str, channel: &str) -> Result<Vec<(i64, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT idx, words_json FROM post_pass_windows
              WHERE meeting_id = ?1 AND channel = ?2 ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![meeting_id, channel], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Checkpoint one transcribed window and the job's progress, together.
    ///
    /// # Errors
    ///
    /// SQLite failures, or a job that is no longer pending.
    pub fn post_pass_checkpoint(
        &mut self,
        meeting_id: &str,
        channel: &str,
        idx: i64,
        words_json: &str,
        progress_permille: i64,
    ) -> Result<()> {
        let tx = self.conn_mut().transaction()?;
        let updated = tx.execute(
            "UPDATE post_pass_jobs SET progress_permille = ?2, updated_at_ms = ?3
              WHERE meeting_id = ?1 AND state = 'pending'",
            params![meeting_id, progress_permille.clamp(0, 1000), now_ms()],
        )?;
        if updated != 1 {
            return Err(StoreError::InvalidArgument("the cleanup job is no longer pending".into()));
        }
        tx.execute(
            "INSERT INTO post_pass_windows (meeting_id, channel, idx, words_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![meeting_id, channel, idx, words_json],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record progress that writes no window (diarization).
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn post_pass_progress(&mut self, meeting_id: &str, progress_permille: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE post_pass_jobs SET progress_permille = ?2, updated_at_ms = ?3
              WHERE meeting_id = ?1 AND state = 'pending'",
            params![meeting_id, progress_permille.clamp(0, 1000), now_ms()],
        )?;
        Ok(())
    }

    /// Store the cleaned transcript as a new version, make it primary, mark
    /// the job done and drop its checkpoints, in one transaction. Returns the
    /// new transcript's id.
    ///
    /// The live version is left exactly as it was. If someone has since made
    /// another transcript primary by hand, the cleaned one does not take over.
    ///
    /// # Errors
    ///
    /// SQLite failures, or a job that is no longer pending.
    pub fn post_pass_complete(
        &mut self,
        meeting_id: &str,
        provider: &str,
        model: &str,
        segments: &[NewSegment],
        provenance_json: &str,
        detail: Option<&str>,
    ) -> Result<String> {
        let tx = self.conn_mut().transaction()?;
        let live: Option<Option<String>> = tx
            .query_row(
                "SELECT live_transcript_id FROM post_pass_jobs
                  WHERE meeting_id = ?1 AND state = 'pending'",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(live) = live else {
            return Err(StoreError::InvalidArgument("the cleanup job is no longer pending".into()));
        };
        let primary: Option<String> = tx
            .query_row(
                "SELECT id FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        let take_over = primary == live;
        let id = insert_transcript(&tx, meeting_id, provider, model, take_over)?;
        insert_segments(&tx, &id, segments)?;
        tx.execute(
            "UPDATE post_pass_jobs SET state = 'done', progress_permille = 1000, updated_at_ms = ?2,
                    cleaned_transcript_id = ?3, provenance_json = ?4, detail = ?5
              WHERE meeting_id = ?1",
            params![meeting_id, now_ms(), id, provenance_json, detail],
        )?;
        tx.execute(
            "DELETE FROM post_pass_windows WHERE meeting_id = ?1",
            params![meeting_id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Mark a job failed and drop its checkpoints. The live version stays.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn post_pass_fail(&mut self, meeting_id: &str, detail: &str) -> Result<()> {
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "UPDATE post_pass_jobs SET state = 'failed', updated_at_ms = ?2, detail = ?3
              WHERE meeting_id = ?1 AND state = 'pending'",
            params![meeting_id, now_ms(), detail],
        )?;
        tx.execute(
            "DELETE FROM post_pass_windows WHERE meeting_id = ?1",
            params![meeting_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Whether summaries and the GitHub push should still wait for this
    /// meeting's cleanup: a pending job younger than [`POST_PASS_WAIT_MS`].
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn post_pass_holds_enrichment(&self, meeting_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {} FROM meetings m WHERE m.id = ?1", HOLDS_ENRICHMENT),
                params![meeting_id],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }
}

/// SQL over `meetings m`: true while a pending cleanup is younger than
/// [`POST_PASS_WAIT_MS`]. Shared with the enrichment queue's filter.
pub(crate) const HOLDS_ENRICHMENT: &str = "EXISTS (SELECT 1 FROM post_pass_jobs j
      WHERE j.meeting_id = m.id AND j.state = 'pending'
        AND j.created_at_ms > CAST(strftime('%s', 'now') AS INTEGER) * 1000 - 900000)";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::DbKey;
    use crate::models::NewMeeting;

    fn db_with_meeting() -> (Db, String, String) {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x46; 32])).unwrap();
        let meeting = db.meetings().create(NewMeeting::new("dev-1", "UTC")).unwrap();
        let live = db
            .meetings()
            .create_transcript(&meeting, "fotw-speechd", "parakeet-unified", true)
            .unwrap();
        db.meetings()
            .append_segments(&live, &[seg(0, "live words")])
            .unwrap();
        (db, meeting, live)
    }

    fn seg(idx: i64, text: &str) -> NewSegment {
        NewSegment {
            idx,
            start_ms: idx * 1_000,
            end_ms: idx * 1_000 + 900,
            channel: "mic".into(),
            speaker_label: Some("me".into()),
            person_id: None,
            text: text.into(),
            confidence: None,
            is_final: true,
            words: None,
        }
    }

    #[test]
    fn the_wait_constant_matches_the_sql() {
        assert!(HOLDS_ENRICHMENT.contains(&POST_PASS_WAIT_MS.to_string()));
    }

    #[test]
    fn completing_adds_a_primary_version_beside_the_untouched_live_one() {
        let (mut db, meeting, live) = db_with_meeting();
        assert!(db.post_pass_queue(&meeting, "{}").unwrap());
        assert!(!db.post_pass_queue(&meeting, "{}").unwrap(), "one job per meeting");
        db.post_pass_checkpoint(&meeting, "mic", 0, "[]", 400).unwrap();
        assert_eq!(db.post_pass_windows(&meeting, "mic").unwrap(), [(0, "[]".into())]);
        assert!(db.post_pass_holds_enrichment(&meeting).unwrap());

        let cleaned = db
            .post_pass_complete(&meeting, "fotw-speechd", "parakeet-unified", &[seg(0, "clean")], r#"{"a":1}"#, None)
            .unwrap();
        assert_eq!(db.meetings().primary_transcript_id(&meeting).unwrap(), Some(cleaned.clone()));
        assert_eq!(db.meetings().transcript_text(&live).unwrap(), [(0, "live words".into())]);
        let job = db.post_pass_job(&meeting).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Done);
        assert_eq!(job.live_transcript_id.as_deref(), Some(live.as_str()));
        assert_eq!(job.cleaned_transcript_id.as_deref(), Some(cleaned.as_str()));
        assert!(db.post_pass_windows(&meeting, "mic").unwrap().is_empty());
        assert!(!db.post_pass_holds_enrichment(&meeting).unwrap());

        // Reversible.
        db.meetings().set_primary_transcript(&live).unwrap();
        assert_eq!(db.meetings().primary_transcript_id(&meeting).unwrap(), Some(live));
    }

    #[test]
    fn a_person_who_switched_versions_meanwhile_keeps_their_choice() {
        let (mut db, meeting, live) = db_with_meeting();
        db.post_pass_queue(&meeting, "{}").unwrap();
        let other = db.meetings().create_transcript(&meeting, "x", "y", true).unwrap();
        db.post_pass_complete(&meeting, "p", "m", &[], "{}", None).unwrap();
        assert_eq!(db.meetings().primary_transcript_id(&meeting).unwrap(), Some(other));
        assert_ne!(live, "");
    }

    #[test]
    fn a_failed_or_stale_job_no_longer_holds_enrichment() {
        let (mut db, meeting, _) = db_with_meeting();
        db.post_pass_queue(&meeting, "{}").unwrap();
        db.conn()
            .execute(
                "UPDATE post_pass_jobs SET created_at_ms = ?1",
                params![now_ms() - POST_PASS_WAIT_MS - 1_000],
            )
            .unwrap();
        assert!(!db.post_pass_holds_enrichment(&meeting).unwrap(), "15 minutes passed");
        assert_eq!(db.post_pass_pending().unwrap(), [meeting.clone()], "but it still resumes");
        db.post_pass_fail(&meeting, "no model").unwrap();
        assert!(db.post_pass_pending().unwrap().is_empty());
        assert!(db.post_pass_checkpoint(&meeting, "mic", 0, "[]", 1).is_err());
    }

    #[test]
    fn deleting_the_meeting_deletes_its_job_and_checkpoints() {
        let (mut db, meeting, _) = db_with_meeting();
        db.post_pass_queue(&meeting, "{}").unwrap();
        db.post_pass_checkpoint(&meeting, "system", 3, r#"[{"t":"secret"}]"#, 10).unwrap();
        db.delete_meeting(&meeting).unwrap();
        let left: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM post_pass_windows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
        assert!(db.post_pass_job(&meeting).unwrap().is_none());
    }
}

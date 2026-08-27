//! `MeetingRepo` — the typed handlers everything above the store goes through.
//!
//! §9.1's rejected pattern is "expose raw SQL to the caller". This is the
//! replacement: every multi-statement operation that has to be atomic is one
//! method here, wrapped in one transaction, so a caller cannot forget the
//! second half.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::error::{Result, StoreError};
use crate::ids::{new_id, now_ms};
use crate::models::{
    Meeting, NewMeeting, NewSegment, NewSummary, NoteAnchor, StoredSegment, Summary,
};

/// Meeting-scoped reads and writes.
///
/// Obtained from [`Db::meetings`]. Holds the connection mutably because most
/// of what it does is transactional.
pub struct MeetingRepo<'a> {
    db: &'a mut Db,
}

impl Db {
    /// Meeting-scoped reads and writes.
    pub fn meetings(&mut self) -> MeetingRepo<'_> {
        MeetingRepo { db: self }
    }
}

impl MeetingRepo<'_> {
    // ------------------------------------------------------------ meetings

    /// Insert a meeting and return its id.
    pub fn create(&mut self, m: NewMeeting) -> Result<String> {
        let id = new_id();
        let now = now_ms();
        self.db.conn().execute(
            "INSERT INTO meetings (
                 id, title, started_at_ms, tz, folder_id, template_id,
                 calendar_uid, calendar_source, meeting_url, app_hint,
                 state, language, disclosed,
                 created_at, updated_at, lamport, origin_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, 0, ?15)",
            params![
                id,
                m.title,
                m.started_at_ms,
                m.tz,
                m.folder_id,
                m.template_id,
                m.calendar_uid,
                m.calendar_source,
                m.meeting_url,
                m.app_hint,
                m.state,
                m.language,
                i64::from(m.disclosed),
                now,
                m.origin_device_id,
            ],
        )?;
        Ok(id)
    }

    /// Fetch one meeting.
    pub fn get(&self, id: &str) -> Result<Meeting> {
        let sql = format!("SELECT {} FROM meetings WHERE id = ?1", Meeting::COLUMNS);
        self.db
            .conn()
            .query_row(&sql, params![id], |r| Ok(Meeting::from_row(r)))
            .optional()?
            .transpose()?
            .ok_or_else(|| StoreError::NotFound {
                kind: "meeting",
                id: id.to_owned(),
            })
    }

    /// Most recent meetings first.
    ///
    /// Ordered by `started_at_ms DESC`, which `meetings_started_idx` serves
    /// directly — the list view is the app's home screen and must not
    /// table-scan a library with ten thousand meetings in it.
    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Meeting>> {
        let sql = format!(
            "SELECT {} FROM meetings ORDER BY started_at_ms DESC, id DESC LIMIT ?1 OFFSET ?2",
            Meeting::COLUMNS
        );
        let conn = self.db.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit, offset], |r| Ok(Meeting::from_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// Move a meeting through the `recording -> transcribing -> ready` states,
    /// bumping `updated_at` and `lamport` the way every mutation must.
    /// Replace the meeting's title.
    ///
    /// Exists for #67: every meeting is created under a timestamp fallback,
    /// and the real title arrives only after an engine has read the
    /// transcript. Touches nothing but the one column.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when there is no such meeting — a silent
    /// no-op here would let an enrichment step believe it renamed something.
    pub fn set_title(&mut self, meeting_id: &str, title: &str) -> Result<()> {
        // The same updated_at/lamport discipline as set_state: a rename is an
        // edit, and an edit that does not bump the clock is invisible to sync.
        let n = self.db.conn().execute(
            "UPDATE meetings SET title = ?2, updated_at = ?3, lamport = lamport + 1
             WHERE id = ?1",
            params![meeting_id, title, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Move the meeting to a new lifecycle state.
    pub fn set_state(&mut self, meeting_id: &str, state: &str) -> Result<()> {
        let n = self.db.conn().execute(
            "UPDATE meetings SET state = ?2, updated_at = ?3, lamport = lamport + 1
             WHERE id = ?1",
            params![meeting_id, state, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Stamp the end of a meeting and denormalise its duration.
    pub fn finish(&mut self, meeting_id: &str, ended_at_ms: i64) -> Result<()> {
        let n = self.db.conn().execute(
            "UPDATE meetings
                SET ended_at_ms = ?2,
                    duration_ms = ?2 - started_at_ms,
                    updated_at = ?3,
                    lamport = lamport + 1
              WHERE id = ?1",
            params![meeting_id, ended_at_ms, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Record what the last enrichment pass found (#74).
    ///
    /// `status` is one of `ok`, `no_engine`, `engine_unresolvable`, `failed`;
    /// `detail` is the reason for the two that have one, and is cleared on the
    /// ones that do not — a healthy status beside a stale reason is worse than
    /// no reason at all.
    ///
    /// # The one mutation here that does not bump the merge triple
    ///
    /// [`Self::set_title`], [`Self::set_state`] and [`Self::finish`] all bump
    /// `updated_at` and `lamport`, and say why: an edit that does not move the
    /// clock is invisible to sync (§9.7 invariant 2). This one deliberately
    /// does not, because it is not an edit to the meeting — it is *this
    /// device's* diagnosis of *this device's* daemon. Whether a `claude` binary
    /// resolves on this laptop says nothing about the user's other one, so
    /// letting it win a merge would overwrite the other machine's real,
    /// possibly successful report with a local failure. The column is derived
    /// state that happens to live on the row.
    ///
    /// # It also rewrites the title's FTS entry, and that is left alone
    ///
    /// `meetings_fts_au` is `AFTER UPDATE ON meetings` for *any* column, so
    /// this statement — which touches neither `title` nor `rowid` — costs a
    /// full FTS delete-and-reinsert of an unchanged title. Correct, just
    /// redundant, and deliberately not fixed (#89): scoping the trigger to
    /// `AFTER UPDATE OF title` means a migration 0004, and a bumped
    /// [`LATEST_SCHEMA_VERSION`](crate::LATEST_SCHEMA_VERSION) makes every
    /// already-shipped binary refuse the library on sight — see
    /// `migrations::migrate`'s refusal and why it is not optional. Paying that
    /// to save three redundant index writes an hour is the wrong trade, and a
    /// mis-scoped trigger on an external-content index desynchronises search
    /// silently rather than loudly. Worth revisiting only alongside a migration
    /// that is earning its keep some other way.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when there is no such meeting.
    pub fn set_enrich_report(
        &mut self,
        meeting_id: &str,
        status: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let n = self.db.conn().execute(
            "UPDATE meetings SET enrich_status = ?2, enrich_detail = ?3 WHERE id = ?1",
            params![meeting_id, status, detail],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Meetings that have something to summarise and no summary, oldest first.
    ///
    /// The backfill sweeper's query (#74). `insert_summary` has one production
    /// caller, so a meeting that missed its single enrichment window — the
    /// daemon was restarting, the engine was not installed yet — stayed
    /// unsummarised forever; 33 of them did.
    ///
    /// `failed` is excluded. A meeting whose engine ran and errored would
    /// otherwise be retried on every pass, which is how an hourly sweeper burns
    /// a subscription in a loop against a usage limit that is not going away.
    /// `fotwd summarize <id>` stays the manual retry for those.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn needing_summary(&self, limit: i64) -> Result<Vec<String>> {
        let conn = self.db.conn();
        let mut stmt = conn.prepare(
            "SELECT m.id FROM meetings m
              WHERE EXISTS (SELECT 1 FROM segments s WHERE s.meeting_id = m.id)
                AND NOT EXISTS (
                      SELECT 1 FROM summaries su
                       WHERE su.meeting_id = m.id AND su.is_current = 1)
                AND (m.enrich_status IS NULL
                     OR m.enrich_status IN ('no_engine', 'engine_unresolvable'))
              ORDER BY m.started_at_ms ASC, m.id ASC
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------- transcripts

    /// Create a transcript for a meeting.
    ///
    /// `make_primary` demotes whatever was primary first, in the same
    /// transaction. Doing the two statements separately would transiently
    /// violate `transcripts_primary_uidx` and fail — which is the index doing
    /// its job, and the reason this is one method rather than two.
    pub fn create_transcript(
        &mut self,
        meeting_id: &str,
        provider: &str,
        model: &str,
        make_primary: bool,
    ) -> Result<String> {
        let id = new_id();
        let now = now_ms();
        let origin: String = self.origin_device_of(meeting_id)?;

        let tx = self.db.conn_mut().transaction()?;
        if make_primary {
            tx.execute(
                "UPDATE transcripts SET is_primary = 0, updated_at = ?2, lamport = lamport + 1
                 WHERE meeting_id = ?1 AND is_primary = 1",
                params![meeting_id, now],
            )?;
        }
        tx.execute(
            "INSERT INTO transcripts (
                 id, meeting_id, provider, model, is_primary,
                 created_at, updated_at, lamport, origin_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0, ?7)",
            params![
                id,
                meeting_id,
                provider,
                model,
                i64::from(make_primary),
                now,
                origin
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// The primary transcript for a meeting, if it has one.
    ///
    /// `Ok(None)` means the meeting genuinely has no transcript yet — a normal
    /// state, since recording without a provider configured is supported. A
    /// real SQLite failure stays an `Err`: two callers previously hand-rolled
    /// this query with `.ok()`, which rendered a database error as "no
    /// transcript" and would have shown an empty meeting instead of a fault.
    pub fn primary_transcript_id(&self, meeting_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.db
            .conn()
            .query_row(
                "SELECT id FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1",
                params![meeting_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Promote a transcript to primary, demoting the incumbent atomically.
    pub fn set_primary_transcript(&mut self, transcript_id: &str) -> Result<()> {
        let now = now_ms();
        let meeting_id: String = self
            .db
            .conn()
            .query_row(
                "SELECT meeting_id FROM transcripts WHERE id = ?1",
                params![transcript_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                kind: "transcript",
                id: transcript_id.to_owned(),
            })?;

        let tx = self.db.conn_mut().transaction()?;
        tx.execute(
            "UPDATE transcripts SET is_primary = 0, updated_at = ?2, lamport = lamport + 1
             WHERE meeting_id = ?1 AND is_primary = 1",
            params![meeting_id, now],
        )?;
        tx.execute(
            "UPDATE transcripts SET is_primary = 1, updated_at = ?2, lamport = lamport + 1
             WHERE id = ?1",
            params![transcript_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Append segments to a transcript in one transaction.
    ///
    /// One transaction for the whole batch, not one per segment: the STT layer
    /// delivers finals in bursts, and a per-row commit in WAL mode is an fsync
    /// each, which is the difference between a meeting that keeps up and one
    /// that does not.
    ///
    /// `meeting_id` is copied onto every segment from the transcript rather
    /// than trusted from the caller, so the denormalised column cannot drift
    /// away from the join it exists to avoid.
    pub fn append_segments(&mut self, transcript_id: &str, segments: &[NewSegment]) -> Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        let meeting_id: String = self
            .db
            .conn()
            .query_row(
                "SELECT meeting_id FROM transcripts WHERE id = ?1",
                params![transcript_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                kind: "transcript",
                id: transcript_id.to_owned(),
            })?;

        let tx = self.db.conn_mut().transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO segments (
                     id, transcript_id, meeting_id, idx, start_ms, end_ms,
                     channel, speaker_label, person_id, text, confidence,
                     is_final, words
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for s in segments {
                stmt.execute(params![
                    new_id(),
                    transcript_id,
                    meeting_id,
                    s.idx,
                    s.start_ms,
                    s.end_ms,
                    s.channel,
                    s.speaker_label,
                    s.person_id,
                    s.text,
                    s.confidence,
                    i64::from(s.is_final),
                    s.words,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every segment of a transcript, in order, as `(idx, text)`.
    pub fn transcript_text(&self, transcript_id: &str) -> Result<Vec<(i64, String)>> {
        let conn = self.db.conn();
        let mut stmt =
            conn.prepare("SELECT idx, text FROM segments WHERE transcript_id = ?1 ORDER BY idx")?;
        let rows = stmt.query_map(params![transcript_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The transcript with everything a reader needs, not just the words.
    ///
    /// [`Self::transcript_text`] exists for the summariser, which wants a flat
    /// wall of text and nothing else. Rendering a transcript for a *human* is a
    /// different problem: without a speaker label you cannot tell a question
    /// from its answer, and without an offset you cannot find the moment
    /// something was said. Both columns were already being written — we ask
    /// Deepgram for `diarize=true` and pay for it — and were simply never read
    /// back, so the UI showed an anonymous monologue.
    pub fn transcript_segments(&self, transcript_id: &str) -> Result<Vec<StoredSegment>> {
        let conn = self.db.conn();
        let mut stmt = conn.prepare(
            "SELECT idx, start_ms, end_ms, speaker_label, text, confidence, channel
               FROM segments WHERE transcript_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![transcript_id], |r| {
            Ok(StoredSegment {
                idx: r.get(0)?,
                start_ms: r.get(1)?,
                end_ms: r.get(2)?,
                speaker: r.get(3)?,
                text: r.get(4)?,
                confidence: r.get(5)?,
                channel: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --------------------------------------------------------------- notes

    /// Create or replace the meeting's note document and its anchors.
    ///
    /// The anchors are replaced wholesale rather than diffed. A block's
    /// `typed_at_ms` is only meaningful together with the `block_text` it was
    /// captured against, so a half-updated anchor set is worse than none: it
    /// would point the augmentation prompt at the wrong span of transcript and
    /// produce a confidently wrong expansion. Both halves land in one
    /// transaction for the same reason.
    ///
    /// Returns the note id.
    pub fn upsert_note(
        &mut self,
        meeting_id: &str,
        body_md: &str,
        anchors: &[NoteAnchor],
    ) -> Result<String> {
        let now = now_ms();
        let origin = self.origin_device_of(meeting_id)?;
        let existing: Option<String> = self
            .db
            .conn()
            .query_row(
                "SELECT id FROM notes WHERE meeting_id = ?1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;

        let note_id = existing.clone().unwrap_or_else(new_id);

        let tx = self.db.conn_mut().transaction()?;
        if existing.is_some() {
            tx.execute(
                "UPDATE notes SET body_md = ?2, updated_at = ?3, lamport = lamport + 1
                 WHERE id = ?1",
                params![note_id, body_md, now],
            )?;
        } else {
            tx.execute(
                "INSERT INTO notes (id, meeting_id, body_md, created_at, updated_at, lamport, origin_device_id)
                 VALUES (?1, ?2, ?3, ?4, ?4, 0, ?5)",
                params![note_id, meeting_id, body_md, now, origin],
            )?;
        }

        tx.execute(
            "DELETE FROM note_anchors WHERE note_id = ?1",
            params![note_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO note_anchors (id, note_id, meeting_id, block_idx, block_text, typed_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for a in anchors {
                stmt.execute(params![
                    new_id(),
                    note_id,
                    meeting_id,
                    a.block_idx,
                    a.block_text,
                    a.typed_at_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(note_id)
    }

    /// The note body and its anchors, in block order.
    pub fn note(&self, meeting_id: &str) -> Result<Option<(String, Vec<NoteAnchor>)>> {
        let conn = self.db.conn();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT id, body_md FROM notes WHERE meeting_id = ?1",
                params![meeting_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((note_id, body_md)) = row else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT block_idx, block_text, typed_at_ms FROM note_anchors
             WHERE note_id = ?1 ORDER BY block_idx",
        )?;
        let rows = stmt.query_map(params![note_id], |r| {
            Ok(NoteAnchor {
                block_idx: r.get(0)?,
                block_text: r.get(1)?,
                typed_at_ms: r.get(2)?,
            })
        })?;
        let mut anchors = Vec::new();
        for row in rows {
            anchors.push(row?);
        }
        Ok(Some((body_md, anchors)))
    }

    // ----------------------------------------------------------- summaries

    /// Append a new summary version and make it the current one.
    ///
    /// Version is `MAX(version) + 1` read inside the transaction, and the
    /// incumbent `is_current` row is demoted in the same transaction. Both
    /// halves are forced by database constraints — `UNIQUE (meeting_id,
    /// version)` and `summaries_current_uidx` — rather than by convention, so
    /// a caller that reimplemented this badly would get an error instead of
    /// two current summaries.
    pub fn insert_summary(&mut self, meeting_id: &str, s: NewSummary) -> Result<Summary> {
        let id = new_id();
        let now = now_ms();

        let tx = self.db.conn_mut().transaction()?;

        // Guard explicitly: a summary for a nonexistent meeting would
        // otherwise surface as an opaque FK violation.
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM meetings WHERE id = ?1",
                params![meeting_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            });
        }

        let version: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM summaries WHERE meeting_id = ?1",
            params![meeting_id],
            |r| r.get(0),
        )?;

        tx.execute(
            "UPDATE summaries SET is_current = 0 WHERE meeting_id = ?1 AND is_current = 1",
            params![meeting_id],
        )?;

        tx.execute(
            "INSERT INTO summaries (
                 id, meeting_id, version, template_id, transcript_id,
                 provider, model, prompt_hash, body_md, coverage,
                 input_tokens, output_tokens, cost_micros,
                 is_current, created_at, origin_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1, ?14, ?15)",
            params![
                id,
                meeting_id,
                version,
                s.template_id,
                s.transcript_id,
                s.provider,
                s.model,
                s.prompt_hash,
                s.body_md,
                s.coverage,
                s.input_tokens,
                s.output_tokens,
                s.cost_micros,
                now,
                s.origin_device_id,
            ],
        )?;

        let sql = format!("SELECT {} FROM summaries WHERE id = ?1", Summary::COLUMNS);
        let out = tx.query_row(&sql, params![id], |r| Ok(Summary::from_row(r)))??;
        tx.commit()?;
        Ok(out)
    }

    /// The summary the UI should show, if there is one.
    pub fn current_summary(&self, meeting_id: &str) -> Result<Option<Summary>> {
        let sql = format!(
            "SELECT {} FROM summaries WHERE meeting_id = ?1 AND is_current = 1",
            Summary::COLUMNS
        );
        self.db
            .conn()
            .query_row(&sql, params![meeting_id], |r| Ok(Summary::from_row(r)))
            .optional()?
            .transpose()
    }

    /// Every summary version, newest first. The history is the point of the
    /// append-only design; nothing is allowed to hide it.
    pub fn summary_versions(&self, meeting_id: &str) -> Result<Vec<Summary>> {
        let sql = format!(
            "SELECT {} FROM summaries WHERE meeting_id = ?1 ORDER BY version DESC",
            Summary::COLUMNS
        );
        let conn = self.db.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![meeting_id], |r| Ok(Summary::from_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    // --------------------------------------------------------------- inner

    /// The device that owns a meeting. Child rows inherit it so that a row's
    /// origin always matches the meeting it belongs to, which is what makes
    /// `(lamport, origin_device_id)` a total order per meeting.
    fn origin_device_of(&self, meeting_id: &str) -> Result<String> {
        self.db
            .conn()
            .query_row(
                "SELECT origin_device_id FROM meetings WHERE id = ?1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            })
    }
}

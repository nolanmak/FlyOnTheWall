//! Encrypted notes captured before a recording has a saved meeting identity.
use crate::{Db, NoteDocument, Result, StoreError};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

/// A draft tied to one capture session and its original start clock.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionNoteDocument {
    /// Capture/WAL identity, not a meeting ID.
    pub session_id: String,
    /// Original capture start in epoch milliseconds.
    pub started_at_ms: i64,
    /// Revision-checked Markdown and first-keystroke anchors.
    pub note: NoteDocument,
}

impl Db {
    /// Save one completed capture and transfer its draft in one transaction.
    /// Retrying the same session and clock returns the original meeting, preserving edits.
    /// A changed clock is rejected rather than attaching notes to another recording.
    pub fn finalize_capture(
        &mut self,
        session: &str,
        mut meeting: crate::NewMeeting,
        ended_at_ms: i64,
        transcript: Option<(&str, &str, &[crate::NewSegment])>,
    ) -> Result<String> {
        validate_session(session)?;
        let started = meeting.started_at_ms;
        if started < 0 || ended_at_ms < started {
            return Err(invalid("invalid recording clock"));
        }
        let elapsed = ended_at_ms - started;
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(link) = read_link(&tx, session)? {
            if link.started != started || link.elapsed != elapsed {
                return Err(invalid("session already attached to a different clock"));
            }
            tx.commit()?;
            return Ok(link.meeting);
        }
        meeting.state = "ready".into();
        let id = crate::repo::insert_meeting(&tx, meeting)?;
        if let Some((provider, model, segments)) = transcript {
            let tid = crate::repo::insert_transcript(&tx, &id, provider, model, true)?;
            crate::repo::insert_segments(&tx, &tid, segments)?;
        }
        tx.execute(
            "UPDATE meetings SET ended_at_ms=?2,duration_ms=?3 WHERE id=?1",
            params![id, ended_at_ms, elapsed],
        )?;
        promote_in_transaction(&tx, session, &id, started, elapsed)?;
        tx.commit()?;
        Ok(id)
    }

    /// Atomically attach a session's notes to its saved meeting, preserving revisions.
    pub fn promote_session_note(
        &mut self,
        session: &str,
        meeting: &str,
        started_at_ms: i64,
        elapsed_ms: i64,
    ) -> Result<()> {
        validate_session(session)?;
        if started_at_ms < 0 || elapsed_ms < 0 {
            return Err(invalid("invalid recording clock"));
        }
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        promote_in_transaction(&tx, session, meeting, started_at_ms, elapsed_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// The saved meeting attached to a session, including sessions without notes.
    pub fn session_note_meeting(&self, session: &str) -> Result<Option<String>> {
        validate_session(session)?;
        Ok(read_link(self.conn(), session)?.map(|link| link.meeting))
    }

    /// Load a durable draft, or None when this session has never saved notes.
    pub fn session_note(&mut self, session: &str) -> Result<Option<SessionNoteDocument>> {
        let tx = self.conn_mut().transaction()?;
        let draft = read(&tx, session)?;
        tx.commit()?;
        Ok(draft)
    }

    /// Save against the observed revision, bounded by the authoritative recording clock.
    ///
    /// # Errors
    /// Stale revision, invalid/beyond-recording anchors, changed start/anchor times,
    /// or database errors. Callers must obtain elapsed time from the recorder.
    pub fn save_session_note(
        &mut self,
        draft: &SessionNoteDocument,
        elapsed_ms: i64,
    ) -> Result<i64> {
        validate_session(&draft.session_id)?;
        crate::notes::validate_note(
            draft.note.revision,
            &draft.note.body_md,
            &draft.note.anchors,
        )?;
        if draft.started_at_ms < 0
            || elapsed_ms < 0
            || draft
                .note
                .anchors
                .iter()
                .any(|a| a.typed_at_ms > elapsed_ms)
        {
            return Err(invalid("invalid recording clock or anchor bounds"));
        }
        let json = serde_json::to_string(&draft.note.anchors)
            .map_err(|_| invalid("invalid draft anchors"))?;
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(link) = read_link(&tx, &draft.session_id)? {
            if link.started != draft.started_at_ms
                || draft
                    .note
                    .anchors
                    .iter()
                    .any(|a| a.typed_at_ms > link.elapsed)
            {
                return Err(invalid("draft exceeds the original recording bounds"));
            }
            let next = draft
                .note
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid("note revision exhausted"))?;
            crate::notes::write_note(
                &tx,
                &link.meeting,
                draft.note.revision,
                next,
                &draft.note.body_md,
                &draft.note.anchors,
            )?;
            tx.commit()?;
            return Ok(next);
        }
        let old = read(&tx, &draft.session_id)?;
        let revision = old.as_ref().map_or(0, |d| d.note.revision);
        if draft.note.revision != revision {
            return Err(StoreError::NoteConflict);
        }
        if let Some(old) = old {
            if old.started_at_ms != draft.started_at_ms {
                return Err(invalid("recording start changed"));
            }
            for a in &draft.note.anchors {
                if old
                    .note
                    .anchors
                    .iter()
                    .any(|b| b.id == a.id && b.typed_at_ms != a.typed_at_ms)
                {
                    return Err(invalid("original anchor offset changed"));
                }
            }
        }
        let next = revision
            .checked_add(1)
            .ok_or_else(|| invalid("note revision exhausted"))?;
        tx.execute("INSERT INTO session_notes(session_id,started_at_ms,revision,body_md,anchors_json,updated_at_ms)
            VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id) DO UPDATE SET
            revision=excluded.revision,body_md=excluded.body_md,anchors_json=excluded.anchors_json,updated_at_ms=excluded.updated_at_ms",
            params![draft.session_id,draft.started_at_ms,next,draft.note.body_md,json,crate::now_ms()])?;
        tx.commit()?;
        Ok(next)
    }
}

fn validate_session(session: &str) -> Result<()> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid("invalid capture session identity"));
    }
    Ok(())
}
fn invalid(message: &str) -> StoreError {
    StoreError::InvalidArgument(message.into())
}
fn read(conn: &rusqlite::Connection, session: &str) -> Result<Option<SessionNoteDocument>> {
    validate_session(session)?;
    if let Some(link) = read_link(conn, session)? {
        return Ok(Some(SessionNoteDocument {
            session_id: session.into(),
            started_at_ms: link.started,
            note: crate::notes::read_note(conn, &link.meeting)?,
        }));
    }
    let row: Option<(i64,i64,String,String)> = conn.query_row(
        "SELECT started_at_ms,revision,body_md,anchors_json FROM session_notes WHERE session_id=?1", [session],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    row.map(|(started_at_ms, revision, body_md, json)| {
        let anchors: Vec<crate::NoteAnchor> = serde_json::from_str(&json)
            .map_err(|_| invalid("stored session anchors are invalid"))?;
        crate::notes::validate_note(revision, &body_md, &anchors)?;
        Ok(SessionNoteDocument {
            session_id: session.into(),
            started_at_ms,
            note: NoteDocument {
                revision,
                body_md,
                anchors,
            },
        })
    })
    .transpose()
}

struct Link {
    meeting: String,
    started: i64,
    elapsed: i64,
}
fn read_link(conn: &rusqlite::Connection, session: &str) -> Result<Option<Link>> {
    Ok(conn.query_row("SELECT meeting_id,started_at_ms,elapsed_ms FROM session_note_links WHERE session_id=?1", [session],
        |r| Ok(Link{meeting:r.get(0)?,started:r.get(1)?,elapsed:r.get(2)?})).optional()?)
}

fn promote_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    meeting: &str,
    started_at_ms: i64,
    elapsed_ms: i64,
) -> Result<()> {
    if let Some(link) = read_link(tx, session)? {
        if link.meeting != meeting || link.started != started_at_ms || link.elapsed != elapsed_ms {
            return Err(invalid(
                "session already attached to a different meeting or clock",
            ));
        }
        return Ok(());
    }
    let target = crate::notes::read_note(tx, meeting)?;
    if target.revision != 0 {
        return Err(StoreError::NoteConflict);
    }
    if let Some(draft) = read(tx, session)? {
        if draft.started_at_ms != started_at_ms
            || draft
                .note
                .anchors
                .iter()
                .any(|a| a.typed_at_ms > elapsed_ms)
        {
            return Err(invalid("draft does not match the recording clock"));
        }
        crate::notes::write_note(
            tx,
            meeting,
            0,
            draft.note.revision,
            &draft.note.body_md,
            &draft.note.anchors,
        )?;
    }
    tx.execute("INSERT INTO session_note_links(session_id,meeting_id,started_at_ms,elapsed_ms) VALUES(?1,?2,?3,?4)",
            params![session,meeting,started_at_ms,elapsed_ms])?;
    tx.execute("DELETE FROM session_notes WHERE session_id=?1", [session])?;
    Ok(())
}

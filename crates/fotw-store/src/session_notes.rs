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
    /// Load a durable draft, or None when this session has never saved notes.
    pub fn session_note(&self, session: &str) -> Result<Option<SessionNoteDocument>> {
        read(self.conn(), session)
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

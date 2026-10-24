//! Revision-checked editing of the encrypted note document.
use crate::{Db, NoteAnchor, Result, StoreError, new_id, now_ms};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::collections::HashSet;

/// One consistent editor snapshot; revision zero means no note exists yet.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteDocument {
    /// Monotonic local revision, distinct from a timestamp.
    pub revision: i64,
    /// User-authored Markdown.
    pub body_md: String,
    /// Recording offsets retained alongside the text.
    pub anchors: Vec<NoteAnchor>,
}

/// Maximum UTF-8 bytes in a note body or its aggregate anchor text.
pub const MAX_NOTE_BYTES: usize = 1024 * 1024;

impl Db {
    /// Read an editor snapshot from a single database transaction.
    ///
    /// # Errors
    /// Missing meeting or database failure.
    pub fn note_document(&mut self, meeting_id: &str) -> Result<NoteDocument> {
        let tx = self.conn_mut().transaction()?;
        meeting_origin(&tx, meeting_id)?;
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT id, body_md, lamport FROM notes WHERE meeting_id=?1",
                [meeting_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let mut result = NoteDocument {
            revision: 0,
            body_md: String::new(),
            anchors: vec![],
        };
        if let Some((id, body, lamport)) = row {
            result.revision = revision(lamport)?;
            result.body_md = body;
            let mut stmt = tx.prepare("SELECT block_idx,block_text,typed_at_ms FROM note_anchors WHERE note_id=?1 ORDER BY block_idx")?;
            result.anchors = stmt
                .query_map([id], |r| {
                    Ok(NoteAnchor::new(
                        r.get(0)?,
                        r.get::<_, String>(1)?,
                        r.get(2)?,
                    ))
                })?
                .collect::<std::result::Result<_, _>>()?;
        }
        tx.commit()?;
        Ok(result)
    }

    /// Save text and anchors only if the expected revision is still current.
    /// Existing non-editor writes also bump `lamport`, invalidating old snapshots.
    ///
    /// # Errors
    /// Missing meeting, stale revision, invalid input or database failure.
    pub fn save_note_document(
        &mut self,
        meeting_id: &str,
        expected: i64,
        body: &str,
        anchors: &[NoteAnchor],
    ) -> Result<i64> {
        let mut indices = HashSet::new();
        if expected < 0
            || body.len() > MAX_NOTE_BYTES
            || anchors.len() > 4096
            || anchors
                .iter()
                .try_fold(0usize, |n, a| n.checked_add(a.block_text.len()))
                .is_none_or(|n| n > MAX_NOTE_BYTES)
            || anchors
                .iter()
                .any(|a| a.block_idx < 0 || a.typed_at_ms < 0 || !indices.insert(a.block_idx))
        {
            return Err(StoreError::InvalidArgument(
                "invalid note size, revision or anchors".into(),
            ));
        }
        // Serialize the compare and replacement across connections/processes.
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let origin = meeting_origin(&tx, meeting_id)?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT id,lamport FROM notes WHERE meeting_id=?1",
                [meeting_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let current = row
            .as_ref()
            .map(|(_, n)| revision(*n))
            .transpose()?
            .unwrap_or(0);
        if expected != current {
            return Err(StoreError::NoteConflict);
        }
        let next = current
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidArgument("note revision exhausted".into()))?;
        let id = row
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(new_id);
        let now = now_ms();
        tx.execute(
            "INSERT INTO notes(id,meeting_id,body_md,created_at,updated_at,lamport,origin_device_id)
             VALUES(?1,?2,?3,?4,?4,?5,?6)
             ON CONFLICT(meeting_id) DO UPDATE SET body_md=excluded.body_md,updated_at=excluded.updated_at,lamport=excluded.lamport",
            params![id,meeting_id,body,now,next-1,origin],
        )?;
        tx.execute("DELETE FROM note_anchors WHERE note_id=?1", [&id])?;
        for a in anchors {
            tx.execute("INSERT INTO note_anchors(id,note_id,meeting_id,block_idx,block_text,typed_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![new_id(),id,meeting_id,a.block_idx,a.block_text,a.typed_at_ms])?;
        }
        tx.commit()?;
        Ok(next)
    }
}

fn revision(lamport: i64) -> Result<i64> {
    lamport
        .checked_add(1)
        .filter(|n| *n > 0)
        .ok_or_else(|| StoreError::InvalidArgument("invalid note revision".into()))
}

fn meeting_origin(conn: &rusqlite::Connection, id: &str) -> Result<String> {
    conn.query_row(
        "SELECT origin_device_id FROM meetings WHERE id=?1",
        [id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound {
        kind: "meeting",
        id: id.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbKey, NewMeeting, SearchQuery};

    #[test]
    fn anchor_write_failure_rolls_back_body_revision_and_search_index() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([1; 32])).unwrap();
        let id = db
            .meetings()
            .create(NewMeeting::new("fixture", "UTC"))
            .unwrap();
        db.save_note_document(
            &id,
            0,
            "retainedword",
            &[NoteAnchor::new(0, "retainedword", 100)],
        )
        .unwrap();
        let before = db.note_document(&id).unwrap();
        db.conn().execute_batch("CREATE TRIGGER fail_anchor BEFORE INSERT ON note_anchors BEGIN SELECT RAISE(ABORT,'injected persistence failure'); END;").unwrap();
        assert!(
            db.save_note_document(
                &id,
                before.revision,
                "discardedword",
                &[NoteAnchor::new(0, "discardedword", 200)]
            )
            .is_err()
        );
        assert_eq!(db.note_document(&id).unwrap(), before);
        assert!(
            !db.search(&SearchQuery::new("retainedword"))
                .unwrap()
                .is_empty()
        );
        assert!(
            db.search(&SearchQuery::new("discardedword"))
                .unwrap()
                .is_empty()
        );
    }
}

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
        let result = read_note(&tx, meeting_id)?;
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
        validate_note(expected, body, anchors)?;
        // Serialize the compare and replacement across connections/processes.
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidArgument("note revision exhausted".into()))?;
        write_note(&tx, meeting_id, expected, next, body, anchors)?;
        tx.commit()?;
        Ok(next)
    }
}

pub(crate) fn read_note(tx: &rusqlite::Connection, meeting_id: &str) -> Result<NoteDocument> {
    meeting_origin(tx, meeting_id)?;
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
        let mut stmt = tx.prepare("SELECT block_idx,block_text,typed_at_ms,id FROM note_anchors WHERE note_id=?1 ORDER BY block_idx")?;
        result.anchors = stmt
            .query_map([id], |r| {
                Ok(NoteAnchor {
                    block_idx: r.get(0)?,
                    block_text: r.get(1)?,
                    typed_at_ms: r.get(2)?,
                    id: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
    }
    Ok(result)
}

pub(crate) fn write_note(
    tx: &rusqlite::Transaction<'_>,
    meeting_id: &str,
    expected: i64,
    next: i64,
    body: &str,
    anchors: &[NoteAnchor],
) -> Result<()> {
    validate_note(expected, body, anchors)?;
    if next <= expected {
        return Err(StoreError::InvalidArgument(
            "invalid next note revision".into(),
        ));
    }
    let origin = meeting_origin(tx, meeting_id)?;
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
    validate_existing_anchors(tx, meeting_id, anchors)?;
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
                params![a.id,id,meeting_id,a.block_idx,a.block_text,a.typed_at_ms])?;
    }
    Ok(())
}

pub(crate) fn validate_note(expected: i64, body: &str, anchors: &[NoteAnchor]) -> Result<()> {
    let mut indices = HashSet::new();
    let mut identities = HashSet::new();
    if expected < 0
        || body.len() > MAX_NOTE_BYTES
        || anchors.len() > 4096
        || anchors
            .iter()
            .try_fold(0usize, |n, a| n.checked_add(a.block_text.len()))
            .is_none_or(|n| n > MAX_NOTE_BYTES)
        || anchors.iter().any(|a| {
            a.block_idx < 0
                || a.typed_at_ms < 0
                || !indices.insert(a.block_idx)
                || a.id.is_empty()
                || a.id.len() > 64
                || !a
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !identities.insert(&a.id)
        })
    {
        return Err(StoreError::InvalidArgument(
            "invalid note size, revision or anchors".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_existing_anchors(
    tx: &rusqlite::Transaction<'_>,
    meeting_id: &str,
    anchors: &[NoteAnchor],
) -> Result<()> {
    for anchor in anchors {
        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT meeting_id,typed_at_ms FROM note_anchors WHERE id=?1",
                [&anchor.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if existing.is_some_and(|(owner, time)| owner != meeting_id || time != anchor.typed_at_ms) {
            return Err(StoreError::InvalidArgument(
                "anchor identity or original offset changed".into(),
            ));
        }
    }
    Ok(())
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
    fn editing_and_reordering_preserve_anchor_identity_and_original_time() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([1; 32])).unwrap();
        let id = db
            .meetings()
            .create(NewMeeting::new("fixture", "UTC"))
            .unwrap();
        db.save_note_document(
            &id,
            0,
            "first\n\nsecond",
            &[
                NoteAnchor::new(0, "first", 100),
                NoteAnchor::new(1, "second", 200),
            ],
        )
        .unwrap();
        let original: String = db
            .conn()
            .query_row(
                "SELECT id FROM note_anchors WHERE meeting_id=?1 AND block_idx=0",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        let mut doc = db.note_document(&id).unwrap();
        doc.anchors[0].block_idx = 1;
        doc.anchors[0].block_text = "first edited".into();
        doc.anchors[1].block_idx = 0;
        db.save_note_document(&id, doc.revision, "second\n\nfirst edited", &doc.anchors)
            .unwrap();
        let (after, time): (String, i64) = db
            .conn()
            .query_row(
                "SELECT id,typed_at_ms FROM note_anchors WHERE meeting_id=?1 AND block_idx=1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, original);
        assert_eq!(time, 100);
    }

    #[test]
    fn changed_offsets_and_cross_meeting_identities_are_rejected_atomically() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([1; 32])).unwrap();
        let first = db
            .meetings()
            .create(NewMeeting::new("first", "UTC"))
            .unwrap();
        let other = db
            .meetings()
            .create(NewMeeting::new("other", "UTC"))
            .unwrap();
        let anchor = NoteAnchor::new(0, "original", 200);
        db.save_note_document(&first, 0, "original", &[anchor])
            .unwrap();
        let before = db.note_document(&first).unwrap();
        let mut changed = before.anchors.clone();
        changed[0].typed_at_ms = 201;
        assert!(matches!(
            db.save_note_document(&first, before.revision, "changed", &changed),
            Err(StoreError::InvalidArgument(_))
        ));
        assert_eq!(db.note_document(&first).unwrap(), before);
        assert!(matches!(
            db.meetings()
                .upsert_note(&first, "legacy changed", &changed),
            Err(StoreError::InvalidArgument(_))
        ));
        assert_eq!(db.note_document(&first).unwrap(), before);
        assert!(matches!(
            db.save_note_document(&other, 0, "copied", &before.anchors),
            Err(StoreError::InvalidArgument(_))
        ));
        assert_eq!(db.note_document(&other).unwrap().revision, 0);
        let mut duplicate = before.anchors.clone();
        duplicate.push(before.anchors[0].clone());
        duplicate[1].block_idx = 1;
        assert!(matches!(
            db.save_note_document(&first, before.revision, "duplicate", &duplicate),
            Err(StoreError::InvalidArgument(_))
        ));
        assert_eq!(db.note_document(&first).unwrap(), before);
    }

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

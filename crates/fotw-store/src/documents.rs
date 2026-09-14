//! Append-only sharing drafts. The payload belongs to the document generator.
use crate::{Db, Result, StoreError, new_id, now_ms};
use rusqlite::{OptionalExtension, params};

impl Db {
    /// Read the newest sharing draft (revision, JSON payload).
    ///
    /// # Errors
    /// A database read failure.
    pub fn sharing_document(&self, meeting_id: &str) -> Result<Option<(i64, String)>> {
        Ok(self.conn().query_row(
            "SELECT version, document_json FROM meeting_documents WHERE meeting_id=?1 ORDER BY version DESC LIMIT 1",
            [meeting_id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?)
    }

    /// Append a revision, refusing a stale editor rather than losing its changes.
    /// `expected` is the revision the caller loaded (zero for no document).
    ///
    /// # Errors
    /// Missing meeting, stale revision, or database failure.
    pub fn save_sharing_document(
        &mut self,
        meeting_id: &str,
        expected: i64,
        json: &str,
    ) -> Result<i64> {
        let tx = self
            .conn_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version),0) FROM meeting_documents WHERE meeting_id=?1",
            [meeting_id],
            |r| r.get(0),
        )?;
        if current != expected {
            return Err(StoreError::NotFound {
                kind: "current document revision",
                id: expected.to_string(),
            });
        }
        tx.execute(
            "INSERT INTO meeting_documents (id,meeting_id,version,document_json,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![new_id(), meeting_id, current+1, json, now_ms()],
        )?;
        tx.commit()?;
        Ok(current + 1)
    }
}

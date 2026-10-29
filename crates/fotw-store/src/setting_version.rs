//! Setting snapshots and optional meeting input generations for publication.
use crate::{Db, Result, StoreError};
use rusqlite::{Connection, OptionalExtension};

// Reserved local concurrency metadata; never transfer it in a library archive.
pub(crate) const MEETING_INPUT_PREFIX: &str = "summary_input_epoch:";

/// A setting observation, including its monotonically incremented row revision.
///
/// Comparing values alone misses a change away and back. This is a concurrency
/// fence within a library, not an authorization token or a quality certificate.
#[derive(Clone, PartialEq, Eq)]
pub struct SettingVersion {
    key: String,
    row: Option<(String, i64)>,
    inputs: Option<(String, String)>,
}

impl std::fmt::Debug for SettingVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingVersion").finish_non_exhaustive()
    }
}

impl SettingVersion {
    /// Bind to this meeting's transcript, notes and relevant context generation.
    /// Guarded publication must check both observations in its write transaction.
    /// This is a concurrency fence, not an authorization token.
    ///
    /// # Errors
    /// The meeting/input generation is missing, damaged or bound elsewhere.
    pub fn with_meeting_inputs(mut self, db: &Db, meeting_id: &str) -> Result<Self> {
        if let Some((bound, _)) = &self.inputs {
            if bound != meeting_id {
                return Err(StoreError::InvalidArgument(
                    "input observation is bound to another meeting".into(),
                ));
            }
            return Ok(self); // Never refresh an already frozen observation.
        }
        let epoch = input_epoch(db.conn(), meeting_id)?.ok_or_else(|| {
            StoreError::InvalidArgument("meeting input generation is unavailable".into())
        })?;
        self.inputs = Some((meeting_id.into(), epoch));
        Ok(self)
    }

    /// The observed JSON value, absent when this key has never been stored.
    pub fn value(&self) -> Option<&str> {
        self.row.as_ref().map(|(value, _)| value.as_str())
    }

    /// Whether the setting and any bound meeting inputs remain current.
    pub fn is_current(&self, db: &Db) -> Result<bool> {
        self.matches(db.conn())
    }

    fn matches(&self, conn: &Connection) -> Result<bool> {
        if self.row != read(conn, &self.key)?.row {
            return Ok(false);
        }
        match &self.inputs {
            Some((meeting, epoch)) => Ok(input_epoch(conn, meeting)?.as_ref() == Some(epoch)),
            None => Ok(true),
        }
    }

    pub(crate) fn ensure_for_meeting(&self, conn: &Connection, meeting_id: &str) -> Result<()> {
        if self
            .inputs
            .as_ref()
            .is_some_and(|(bound, _)| bound != meeting_id)
        {
            return Err(StoreError::InvalidArgument(
                "generated output targets a different meeting".into(),
            ));
        }
        self.ensure_current(conn)
    }

    pub(crate) fn ensure_current(&self, conn: &Connection) -> Result<()> {
        if self.matches(conn)? {
            Ok(())
        } else {
            Err(StoreError::InvalidArgument(
                "summary settings or meeting inputs changed before generated output could be saved"
                    .into(),
            ))
        }
    }
}

impl Db {
    /// Observe a setting's value and revision in one SQLite read.
    ///
    /// Use the guarded publication methods to check this observation within
    /// the same transaction as the write; a separate check is not atomic.
    pub fn setting_version(&self, key: &str) -> Result<SettingVersion> {
        read(self.conn(), key)
    }
}

fn read(conn: &Connection, key: &str) -> Result<SettingVersion> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT value,lamport FROM settings WHERE key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if row.as_ref().is_some_and(|(_, revision)| *revision < 0) {
        return Err(StoreError::InvalidArgument(
            "invalid setting revision".into(),
        ));
    }
    Ok(SettingVersion {
        key: key.into(),
        row,
        inputs: None,
    })
}

fn input_epoch(conn: &Connection, meeting_id: &str) -> Result<Option<String>> {
    // The join refuses an orphan metadata row after meeting deletion. Read a
    // constant number of indexed rows, not the transcript on every guard tick.
    let value: Option<String> = conn
        .query_row(
            "SELECT a.value FROM app_meta a JOIN meetings m ON m.id=?1 WHERE a.key=?2",
            rusqlite::params![meeting_id, format!("{MEETING_INPUT_PREFIX}{meeting_id}")],
            |row| row.get(0),
        )
        .optional()?;
    if value.as_ref().is_some_and(|epoch| {
        epoch.len() != 64
            || !epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(StoreError::InvalidArgument(
            "meeting input generation is invalid".into(),
        ));
    }
    Ok(value)
}

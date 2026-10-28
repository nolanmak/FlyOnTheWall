//! Exact setting value/revision snapshots for generated-result publication.
use crate::{Db, Result, StoreError};
use rusqlite::{Connection, OptionalExtension};

/// A setting observation, including its monotonically incremented row revision.
///
/// Comparing values alone misses a change away and back. This is a concurrency
/// fence within a library, not an authorization token or a quality certificate.
#[derive(Clone, PartialEq, Eq)]
pub struct SettingVersion {
    key: String,
    row: Option<(String, i64)>,
}

impl std::fmt::Debug for SettingVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingVersion").finish_non_exhaustive()
    }
}

impl SettingVersion {
    /// The observed JSON value, absent when this key has never been stored.
    pub fn value(&self) -> Option<&str> {
        self.row.as_ref().map(|(value, _)| value.as_str())
    }

    /// Whether neither the value nor its revision has changed in this library.
    pub fn is_current(&self, db: &Db) -> Result<bool> {
        Ok(*self == read(db.conn(), &self.key)?)
    }

    pub(crate) fn ensure_current(&self, conn: &Connection) -> Result<()> {
        if *self == read(conn, &self.key)? {
            Ok(())
        } else {
            Err(StoreError::InvalidArgument(
                "settings changed before generated output could be saved".into(),
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
    })
}

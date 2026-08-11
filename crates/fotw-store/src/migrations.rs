//! Forward-only schema migrations, and the refusal that protects a library
//! from an older binary.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

use crate::error::{Result, StoreError};
use crate::ids::now_ms;
use crate::key::DbKey;

/// The highest `PRAGMA user_version` this build understands.
///
/// Bump this in the same commit that appends to [`migration_set`]; the two
/// drifting apart is what [`crate::Db::open`]'s refusal is guarding against.
pub const LATEST_SCHEMA_VERSION: usize = 1;

/// Every migration, oldest first. Append only — a released migration is
/// immutable, because editing one changes the schema of databases that already
/// recorded it as applied.
fn migration_set() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("schema/0001_initial.sql")).comment("initial schema"),
    ])
}

/// Read `PRAGMA user_version`.
pub(crate) fn user_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(usize::try_from(v).unwrap_or(0))
}

/// Bring `conn` up to [`LATEST_SCHEMA_VERSION`], refusing to touch anything
/// newer and taking a backup first when there is something to lose.
///
/// # Why the refusal is not optional
///
/// Migrations are forward-only: there are no `down` scripts, on purpose, since
/// a down-migration for a schema change that drops information cannot restore
/// it. So an older binary opening a newer database has exactly two options —
/// refuse, or run this-version SQL against next-version tables. The second one
/// looks like it works: `SELECT` against a table that gained a column succeeds,
/// and the damage only shows up later as rows written without the new column's
/// invariants. By then the user has been running the old build for a week and
/// no backup taken *after* the downgrade contains a clean copy. Refusing is the
/// only choice that keeps the data recoverable.
///
/// # Why the backup is taken here
///
/// Right before the migration transaction, and only when there is existing
/// data — a fresh database has nothing worth backing up. See
/// [`backup_before_migration`] for why it is not a file copy.
pub(crate) fn migrate(
    conn: &mut Connection,
    db_path: Option<&Path>,
    key: Option<&DbKey>,
) -> Result<()> {
    let found = user_version(conn)?;

    if found > LATEST_SCHEMA_VERSION {
        return Err(StoreError::DatabaseTooNew {
            found,
            supported: LATEST_SCHEMA_VERSION,
        });
    }

    if found == LATEST_SCHEMA_VERSION {
        return Ok(());
    }

    if found > 0
        && let Some(path) = db_path
    {
        backup_before_migration(conn, path, found, key)?;
    }

    migration_set().to_latest(conn)?;
    Ok(())
}

/// Snapshot the database into `<root>/backups/pre-migration-<n>-<ts>.db`.
///
/// Uses SQLite's online-backup API rather than `std::fs::copy`, for two
/// reasons. First, in WAL mode the main file is not a complete database — a
/// copy that misses the `-wal` is a copy of the state at the last checkpoint,
/// which is precisely the data a user would be trying to recover. Second, the
/// destination connection is keyed *before* the first page is written, so the
/// backup is SQLCipher-encrypted like the original. A plain copy of a keyed
/// database through the un-keyed backup API would write the whole library out
/// in plaintext, which is a far worse bug than the one this function exists to
/// mitigate.
fn backup_before_migration(
    conn: &Connection,
    db_path: &Path,
    from_version: usize,
    key: Option<&DbKey>,
) -> Result<PathBuf> {
    let dir = backups_dir(db_path);
    std::fs::create_dir_all(&dir).map_err(|e| StoreError::io("creating backups dir", &dir, e))?;

    let dest = dir.join(format!(
        "pre-migration-{from_version}-{ts}.db",
        ts = now_ms()
    ));

    let mut to = Connection::open(&dest)?;
    if let Some(key) = key {
        crate::db::apply_key(&to, key)?;
    }

    let backup = rusqlite::backup::Backup::new(conn, &mut to)?;
    // Big steps and no pause: nobody else has this database open yet at
    // `open()` time, so there is no reader to be polite to, and the user is
    // staring at a progress spinner.
    backup.run_to_completion(1024, std::time::Duration::ZERO, None)?;
    drop(backup);
    to.close().map_err(|(_, e)| StoreError::Sqlite(e))?;

    Ok(dest)
}

/// `<root>/backups/`, per the on-disk layout in §9.2.
pub(crate) fn backups_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

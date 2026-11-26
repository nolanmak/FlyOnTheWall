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
pub const LATEST_SCHEMA_VERSION: usize = 13;

/// Every migration, oldest first. Append only — a released migration is
/// immutable, because editing one changes the schema of databases that already
/// recorded it as applied.
///
/// 0002 is a separate file rather than an edit to 0001 for exactly that reason:
/// 0001 has shipped, so a released library has to migrate *forward* into the
/// FTS tables (and backfill them — see the `'rebuild'` statements at the end of
/// 0002) rather than be rewritten underneath.
fn migration_set(new_library: bool) -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("schema/0001_initial.sql")).comment("initial schema"),
        M::up(include_str!("schema/0002_fts.sql")).comment("fts5 search indexes"),
        M::up(include_str!("schema/0003_enrich_report.sql")).comment("enrich report on meetings"),
        M::up(include_str!("schema/0004_documents.sql")).comment("versioned sharing documents"),
        M::up(include_str!("schema/0005_session_notes.sql"))
            .comment("encrypted recording note drafts"),
        M::up(include_str!("schema/0006_session_note_links.sql"))
            .comment("session note handoff aliases"),
        M::up(include_str!("schema/0007_speech_receipts.sql"))
            .comment("durable speech receipts and provenance"),
        M::up(include_str!("schema/0008_capture_clock_state.sql"))
            .comment("explicit interrupted capture clocks"),
        M::up_with_hook(
            include_str!("schema/0009_summary_destination_bootstrap.sql"),
            move |tx| {
                if new_library {
                    // Bootstrap defaults are identical across fresh libraries.
                    // A user save gets its own timestamp/lamport revision; the
                    // untouched seed must round-trip without false conflicts.
                    tx.execute(
                        "INSERT INTO settings
                            (key,value,updated_at,lamport,origin_device_id)
                         VALUES('summarize',?1,0,0,'local')",
                        [r#"{"policy":"off"}"#],
                    )?;
                    tx.execute(
                        "UPDATE app_meta SET value='explicit-off-v1'
                         WHERE key='summary_destination_bootstrap'",
                        [],
                    )?;
                }
                Ok(())
            },
        )
        .comment("explicit summary destination defaults for new libraries"),
        M::up(include_str!("schema/0010_summary_input_generations.sql"))
            .comment("local meeting input generations for stale-result fencing"),
        M::up(include_str!("schema/0011_post_pass.sql"))
            .comment("post-meeting cleanup pass jobs and checkpoints"),
        M::up(include_str!("schema/0012_attendee_release.sql"))
            .comment("device-local attendee release rows, decisions and ledger"),
        M::up_with_hook(
            include_str!("schema/0013_segment_meeting_ownership.sql"),
            |tx| check_references(tx, OWNERSHIP_REBUILT_TABLES),
        )
        .comment("a transcript's rows belong to the transcript's meeting"),
    ])
}

/// The tables 0013 rebuilds, which therefore run their copy with foreign keys
/// switched off (see [`migrate`]) and are checked by hand before commit.
const OWNERSHIP_REBUILT_TABLES: &[&str] = &[
    "segments",
    "speech_epochs",
    "speech_receipts",
    "summaries",
    "post_pass_jobs",
];

/// Fail the migration if any row of `tables` violates a foreign key.
///
/// The table-rebuild migrations run with `PRAGMA foreign_keys = OFF`, so this
/// is the only thing standing between a bad copy and a committed library that
/// breaks its own references. Scoped to the rebuilt tables on purpose: a
/// whole-database check would refuse to open a library over an unrelated
/// legacy row this migration never touched.
fn check_references(
    tx: &rusqlite::Transaction<'_>,
    tables: &[&str],
) -> std::result::Result<(), rusqlite_migration::HookError> {
    for table in tables {
        let mut stmt = tx.prepare(&format!("PRAGMA foreign_key_check(\"{table}\")"))?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let parent: String = row.get(2)?;
            return Err(rusqlite_migration::HookError::Hook(format!(
                "after rebuilding {table}, a row still violates its reference to {parent}"
            )));
        }
    }
    Ok(())
}

/// The library key prefix 0013 writes its repair counts under.
pub(crate) const OWNERSHIP_REPAIR_PREFIX: &str = "ownership_repair:";

/// What 0013 found and did in one table of a library that already held rows
/// whose meeting disagreed with their transcript's meeting (#343).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct OwnershipRepair {
    /// The table the rows were in.
    #[serde(skip)]
    pub table: String,
    /// Mismatched rows found (for `post_pass_jobs`, mismatched pointers).
    pub found: u64,
    /// Rows moved to their transcript's meeting.
    pub rehomed: u64,
    /// Rows deleted because no single owner could be proven.
    pub removed: u64,
    /// Transcript pointers set to NULL.
    pub cleared: u64,
}

impl crate::Db {
    /// The repairs migration 0013 made, one entry per table where it found
    /// anything; empty for a library that never held a mismatched row.
    ///
    /// # Errors
    ///
    /// A database error, or a report that does not parse.
    pub fn ownership_repairs(&self) -> Result<Vec<OwnershipRepair>> {
        let mut stmt = self.conn().prepare(
            "SELECT key, value FROM app_meta WHERE key GLOB 'ownership_repair:*' ORDER BY key",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value) = row?;
            let mut repair: OwnershipRepair = serde_json::from_str(&value).map_err(|e| {
                StoreError::InvalidArgument(format!("unreadable {key} report: {e}"))
            })?;
            repair.table = key[OWNERSHIP_REPAIR_PREFIX.len()..].to_owned();
            out.push(repair);
        }
        Ok(out)
    }
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

    // Back up whenever there is anything to lose. Keying off `found > 0` would
    // look equivalent and is not: a database can hold objects at
    // `user_version = 0` — one written before migrations existed, or one
    // recovered by hand — and those are exactly the files where a migration is
    // most likely to go wrong and a backup most likely to be needed.
    let contains_objects = has_objects(conn)?;
    let new_library = found == 0 && !contains_objects;
    if let Some(path) = db_path
        && contains_objects
    {
        backup_before_migration(conn, path, found, key)?;
    }

    // Table-rebuild migrations (0013) drop and recreate tables that others
    // reference. With enforcement on, that DROP runs an implicit DELETE whose
    // ON DELETE CASCADE would empty every child table. SQLite's documented
    // procedure is to switch enforcement off for the rebuild, which has to
    // happen here: inside the migration transaction the pragma is a no-op.
    // Each rebuilding migration checks its own tables before commit, and
    // enforcement comes back on whether the run succeeded or not.
    conn.pragma_update(None, "foreign_keys", false)?;
    let migrated = migration_set(new_library).to_latest(conn);
    conn.pragma_update(None, "foreign_keys", true)?;
    migrated?;
    Ok(())
}

/// True when the database contains anything at all.
fn has_objects(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Snapshot the database into `<root>/backups/pre-migration-<n>-<ts>.db`,
/// where `<n>` is the version the file is being migrated *from* — the version
/// of the data inside the backup, which is the only reading that helps someone
/// deciding whether a given file is the one they want to restore.
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rusqlite::Connection;

    use super::{LATEST_SCHEMA_VERSION, OWNERSHIP_REBUILT_TABLES, migration_set};

    /// Every trigger, index and column on the tables 0013 rebuilds.
    fn shape(conn: &Connection) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for table in OWNERSHIP_REBUILT_TABLES {
            let mut stmt = conn
                .prepare(
                    "SELECT type || ' ' || name FROM sqlite_schema
                     WHERE tbl_name = ?1 AND type IN ('index', 'trigger')
                       AND name NOT LIKE 'sqlite_autoindex_%'",
                )
                .unwrap();
            for name in stmt.query_map([table], |r| r.get::<_, String>(0)).unwrap() {
                out.insert(name.unwrap());
            }
            let mut stmt = conn
                .prepare(
                    "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)",
                )
                .unwrap();
            let cols = stmt
                .query_map([table], |r| {
                    Ok(format!(
                        "column {table}.{} {} nn={} default={:?} pk={}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })
                .unwrap();
            for col in cols {
                out.insert(col.unwrap());
            }
        }
        out
    }

    /// The rebuild recreates by hand everything that dropping a table takes
    /// with it. Measured against whatever the migration before it left, so a
    /// trigger, index or column another migration adds to one of these tables
    /// fails here rather than vanishing from users' libraries.
    #[test]
    fn the_ownership_rebuild_keeps_every_trigger_index_and_column() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = migration_set(true);
        migrations
            .to_version(&mut conn, LATEST_SCHEMA_VERSION - 1)
            .unwrap();
        let before = shape(&conn);
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        migrations.to_latest(&mut conn).unwrap();
        let after = shape(&conn);
        assert!(
            before.is_subset(&after),
            "lost by the rebuild: {:?}",
            before.difference(&after).collect::<Vec<_>>()
        );
        assert_eq!(
            after.difference(&before).collect::<Vec<_>>(),
            Vec::<&String>::new(),
            "the rebuild should add references, not columns, triggers or indexes"
        );
    }
}

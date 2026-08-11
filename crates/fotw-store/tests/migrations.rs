//! Migrations are forward-only, tracked in `PRAGMA user_version`, backed up
//! before they run, and refused outright when the file is newer than the
//! binary (docs/REQUIREMENTS.md 9.1).

use fotw_store::{Db, LATEST_SCHEMA_VERSION, StoreError};
use rusqlite::Connection;

mod common;
use common::{test_key, tmp_db};

/// Open a raw connection the way [`Db`] does, to build "old" or "future"
/// databases the public API deliberately cannot produce.
fn raw(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA key = \"x'{}'\";PRAGMA cipher_page_size = 4096;",
        "01".repeat(32)
    ))
    .unwrap();
    conn
}

#[test]
fn a_fresh_database_lands_on_the_latest_version() {
    let (_dir, path) = tmp_db();
    let db = Db::open(&path, &test_key()).unwrap();
    assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    assert_eq!(LATEST_SCHEMA_VERSION, 1, "migration 0001 is the only one");
}

/// Reopening is a no-op, not a re-run. If migrations were re-applied the
/// second `CREATE TABLE meetings` would fail, so this also proves the version
/// gate is what is short-circuiting rather than `IF NOT EXISTS` hiding a bug.
#[test]
fn opening_an_up_to_date_database_changes_nothing() {
    let (_dir, path) = tmp_db();
    {
        let db = Db::open(&path, &test_key()).unwrap();
        db.conn()
            .execute(
                "INSERT INTO meetings (id, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
                 VALUES ('m1', 0, 'UTC', 'ready', 0, 0, 'd1')",
                [],
            )
            .unwrap();
    }

    let db = Db::open(&path, &test_key()).unwrap();
    assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    let n: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM meetings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "reopening must not have rebuilt the schema");
}

/// The refusal. A binary that opened a `user_version` it does not understand
/// would run this-version SQL against next-version tables; the failure mode is
/// not a crash but a week of quiet corruption, by which point no backup taken
/// after the downgrade is clean.
#[test]
fn a_database_from_a_newer_build_is_refused() {
    let (_dir, path) = tmp_db();
    Db::open(&path, &test_key()).unwrap();
    {
        let conn = raw(&path);
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        conn.execute(
            "INSERT INTO meetings (id, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
             VALUES ('from-the-future', 0, 'UTC', 'ready', 0, 0, 'd1')",
            [],
        )
        .unwrap();
    }

    let err = Db::open(&path, &test_key()).unwrap_err();
    match &err {
        StoreError::DatabaseTooNew { found, supported } => {
            assert_eq!(*found, 99);
            assert_eq!(*supported, LATEST_SCHEMA_VERSION);
        }
        other => panic!("expected DatabaseTooNew, got {other:?}"),
    }
    assert!(err.is_database_too_new());
    assert!(
        err.to_string().contains("newer version"),
        "the message has to tell the user to upgrade the app: {err}"
    );

    // And the refusal must not have touched anything on the way out.
    let conn = raw(&path);
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 99, "a refused open must not downgrade the file");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM meetings WHERE id = 'from-the-future'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "a refused open must not touch the data");
}

/// A database with content but nothing applied yet gets snapshotted first.
///
/// Asserts three separate things about the backup, because a backup that fails
/// any one of them is not a backup: it exists, it is *encrypted*, and it still
/// contains the pre-migration data.
#[test]
fn migrating_a_database_with_content_takes_an_encrypted_backup_first() {
    let (dir, path) = tmp_db();
    {
        let conn = raw(&path);
        conn.execute_batch(
            "CREATE TABLE legacy (id TEXT PRIMARY KEY, note TEXT);
             INSERT INTO legacy VALUES ('a', 'irreplaceable');",
        )
        .unwrap();
    }

    let db = Db::open(&path, &test_key()).unwrap();
    assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    drop(db);

    let backups: Vec<_> = std::fs::read_dir(dir.path().join("backups"))
        .expect("a backups/ directory must exist")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1, "expected exactly one backup: {backups:?}");

    let name = backups[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        name.starts_with("pre-migration-0-") && name.ends_with(".db"),
        "backup is misnamed: {name}"
    );

    let bytes = std::fs::read(&backups[0]).unwrap();
    assert!(
        !bytes.starts_with(b"SQLite format 3\0"),
        "the backup is plaintext -- it would leak the whole library"
    );

    let restored = raw(&backups[0]);
    let note: String = restored
        .query_row("SELECT note FROM legacy WHERE id = 'a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(note, "irreplaceable");
    let v: i64 = restored
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 0, "the backup holds the pre-migration state");
}

/// A brand-new database has nothing to lose, so it must not litter the data
/// root with an empty backup on every first run.
#[test]
fn creating_a_fresh_database_takes_no_backup() {
    let (dir, path) = tmp_db();
    Db::open(&path, &test_key()).unwrap();
    assert!(
        !dir.path().join("backups").exists(),
        "a fresh library has nothing worth backing up"
    );
}

/// `Db::open` creates the data root if it is missing. First run on a new
/// machine is otherwise a `unable to open database file` with no explanation.
#[test]
fn open_creates_the_data_root() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("nested").join("deeper").join("db.sqlite3");
    let db = Db::open(&path, &test_key()).unwrap();
    assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    assert!(path.exists());
}

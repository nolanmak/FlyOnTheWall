//! Connection-bootstrap acceptance tests (docs/REQUIREMENTS.md 9.1).
//!
//! The PRAGMA sequence is not a style preference — three of its steps are
//! one-shot, and getting the order wrong fails silently rather than loudly:
//!
//! * `PRAGMA key` must be the first operation after open, or SQLCipher reads
//!   the file as plaintext and reports "file is not a database".
//! * `PRAGMA foreign_keys` defaults to OFF and is a no-op inside a
//!   transaction, so if it is not set here the whole delete cascade in §9.6
//!   quietly does nothing.
//! * `PRAGMA auto_vacuum` can only move off `NONE` while the database is still
//!   empty; miss the window and only a full `VACUUM` can recover it.
//!
//! Every one of those has to be observable after `open()`, which is what these
//! tests assert.

use fotw_store::{Db, DbKey};

mod common;
use common::{test_key, tmp_db};

/// `PRAGMA foreign_keys` is OFF by default in SQLite and cannot be turned on
/// inside a transaction. Our entire "delete this meeting" story (§9.6) is
/// `ON DELETE CASCADE`, so a connection that skipped this pragma would delete
/// the meeting row and orphan every segment, note and summary under it.
#[test]
fn open_leaves_foreign_keys_on() {
    let (_dir, path) = tmp_db();
    let db = Db::open(&path, &test_key()).unwrap();

    let fk: i64 = db
        .conn()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        fk, 1,
        "foreign keys must be ON for the delete cascade to fire"
    );
}

/// WAL is what lets the N reader connections in §9.1 keep reading while the
/// single writer is mid-meeting. It is also a persistent, file-level setting,
/// so this asserts it survives being set once.
#[test]
fn open_leaves_journal_mode_wal() {
    let (_dir, path) = tmp_db();
    let db = Db::open(&path, &test_key()).unwrap();

    let mode: String = db
        .conn()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");

    // Persistent: a second open of the same file still reports WAL.
    drop(db);
    let db = Db::open(&path, &test_key()).unwrap();
    let mode: String = db
        .conn()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

/// `auto_vacuum = INCREMENTAL` (2) is the one pragma with a hard deadline: it
/// has to land before the first table exists. If this ever regresses,
/// `PRAGMA incremental_vacuum` in `delete_meeting` silently becomes a no-op
/// and deleted pages stay in the file — which is exactly the thing §9.6
/// promises does not happen.
#[test]
fn open_enables_incremental_auto_vacuum() {
    let (_dir, path) = tmp_db();
    let db = Db::open(&path, &test_key()).unwrap();

    let av: i64 = db
        .conn()
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        av, 2,
        "auto_vacuum must be INCREMENTAL(2); it cannot be enabled after the \
         first CREATE TABLE without a full VACUUM"
    );
}

/// The remaining durability/latency pragmas from §9.1. `secure_delete` is the
/// load-bearing one: without it SQLite leaves freed page contents in the file
/// and the §9.6 byte-scan acceptance test cannot pass.
#[test]
fn open_applies_the_rest_of_the_pragma_sequence() {
    let (_dir, path) = tmp_db();
    let db = Db::open(&path, &test_key()).unwrap();
    let c = db.conn();

    let synchronous: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
    assert_eq!(synchronous, 1, "NORMAL");

    let secure_delete: i64 = c
        .query_row("PRAGMA secure_delete", [], |r| r.get(0))
        .unwrap();
    assert_eq!(secure_delete, 1, "secure_delete must be ON");

    let temp_store: i64 = c.query_row("PRAGMA temp_store", [], |r| r.get(0)).unwrap();
    assert_eq!(temp_store, 2, "MEMORY");

    let busy_timeout: i64 = c
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(busy_timeout, 5_000);

    // SQLCipher answers this one as text, not an integer.
    let page_size: String = c
        .query_row("PRAGMA cipher_page_size", [], |r| r.get(0))
        .unwrap();
    assert_eq!(page_size, "4096");
}

/// The file on disk must not be a readable SQLite database. This is the whole
/// point of SQLCipher: §10's threat model is unencrypted backups and other
/// user-space apps, both of which see the raw bytes.
#[test]
fn the_database_file_is_encrypted_at_rest() {
    let (_dir, path) = tmp_db();
    {
        let db = Db::open(&path, &test_key()).unwrap();
        db.conn()
            .execute("INSERT INTO settings (key, value, updated_at, lamport, origin_device_id) VALUES ('x','y',0,0,'d')", [])
            .unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.is_empty());
    assert!(
        !bytes.starts_with(b"SQLite format 3\0"),
        "an encrypted database must not carry the plaintext SQLite header"
    );
}

/// Opening with the wrong key must fail, not succeed-and-return-nothing.
#[test]
fn open_with_the_wrong_key_fails() {
    let (_dir, path) = tmp_db();
    Db::open(&path, &test_key()).unwrap();

    let wrong = DbKey::from_bytes([0x99; 32]);
    let err = Db::open(&path, &wrong).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("not a database")
            || err.to_string().to_lowercase().contains("encrypt"),
        "unexpected error for a bad key: {err}"
    );
}

/// A key must never reach a log line. §10's never-log rules are enforced by
/// types, and this is that type's test.
#[test]
fn the_key_redacts_itself_in_debug_output() {
    let key = test_key();
    let rendered = format!("{key:?}");
    assert!(
        !rendered.contains("0101"),
        "DbKey must not print its material: {rendered}"
    );
    assert!(rendered.contains("redacted"), "got {rendered}");
}

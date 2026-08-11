//! The connection, and the bootstrap sequence that is the only way to get one.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{Result, StoreError};
use crate::key::DbKey;
use crate::migrations;

/// An open, bootstrapped, migrated FlyOnTheWall library.
///
/// # Why this type exists at all
///
/// It exists so that [`Db::open`] is the *only* way to obtain a connection to
/// a library. Every rule in docs/REQUIREMENTS.md 9.1 is a pragma that fails
/// **silently** when it is skipped:
///
/// * miss `PRAGMA key` and SQLCipher reports "file is not a database" — loud,
///   but only for an existing file; on a fresh one you get an unencrypted
///   library and no error at all;
/// * miss `PRAGMA foreign_keys` and every `ON DELETE CASCADE` in the schema
///   stops firing, so `delete_meeting` leaves the transcript behind;
/// * miss `PRAGMA secure_delete` and deleted text stays legible in the file;
/// * miss `PRAGMA auto_vacuum` on the very first open and it can never be
///   enabled again without a full `VACUUM` of the whole library.
///
/// None of those is caught by a code review of the call site, because the call
/// site looks fine. Making the connection unconstructible except through here
/// is what makes them uncatchable-by-omission instead.
pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No connection internals, and above all no key.
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

impl Db {
    /// Open (creating if needed) the library at `path`, then migrate it to
    /// [`migrations::LATEST_SCHEMA_VERSION`].
    ///
    /// Refuses with [`StoreError::DatabaseTooNew`] if the file was written by
    /// a newer build. Takes a `<root>/backups/pre-migration-*.db` snapshot
    /// first whenever there is existing data to migrate.
    pub fn open(path: impl AsRef<Path>, key: &DbKey) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::io("creating the data root", parent, e))?;
        }

        let conn = Connection::open(&path)?;
        let mut db = Self::finish_open(conn, Some(path), Some(key))?;
        migrations::migrate(&mut db.conn, db.path.as_deref(), Some(key))?;
        Ok(db)
    }

    /// An in-memory library, bootstrapped and migrated identically.
    ///
    /// For tests and for the schema-lint pass. There is no backup step because
    /// there is no file to back up.
    pub fn open_in_memory(key: &DbKey) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut db = Self::finish_open(conn, None, Some(key))?;
        migrations::migrate(&mut db.conn, None, Some(key))?;
        Ok(db)
    }

    /// An unencrypted library on disk, for the one test that needs to look at
    /// raw bytes.
    ///
    /// Not public, and not reachable outside `cfg(test)`: an unencrypted
    /// meeting library is exactly the artifact §10 exists to prevent, so the
    /// only way to produce one must be a code path that does not ship. Its
    /// reason to exist is [`crate::delete`]'s byte-scan test, where the cipher
    /// would otherwise make the assertion vacuous.
    #[cfg(test)]
    pub(crate) fn open_plaintext(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        let mut db = Self::finish_open(conn, Some(path), None)?;
        migrations::migrate(&mut db.conn, db.path.as_deref(), None)?;
        Ok(db)
    }

    /// Borrow the underlying connection for reads and ad-hoc statements.
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable borrow, for callers that need to start a transaction.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The file this library lives in, or `None` for an in-memory one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// `PRAGMA user_version` — the applied migration count.
    pub fn schema_version(&self) -> Result<usize> {
        migrations::user_version(&self.conn)
    }

    /// Run the pragma sequence and prove the key is right.
    fn finish_open(conn: Connection, path: Option<PathBuf>, key: Option<&DbKey>) -> Result<Self> {
        bootstrap(&conn, key)?;
        Ok(Self { conn, path })
    }
}

/// Set `PRAGMA key` (and the page size it pairs with) on a freshly opened
/// connection.
///
/// Split out from [`bootstrap`] because the pre-migration backup in
/// [`crate::migrations`] has to key its destination connection too, and
/// `PRAGMA key` must be the first statement on *that* connection as well —
/// SQLCipher decides whether a database is encrypted from the state at the
/// moment page 1 is first written.
pub(crate) fn apply_key(conn: &Connection, key: &DbKey) -> Result<()> {
    let mut stmt = format!("PRAGMA key = \"{}\";", key.pragma_literal());
    let result = conn.execute_batch(&stmt);
    zeroize(&mut stmt);
    result?;

    // 4096 matches SQLCipher 4's default and the OS page size we actually run
    // on. It is set immediately after the key because it is part of the cipher
    // configuration: changing it later on an existing database makes it
    // unreadable.
    conn.execute_batch("PRAGMA cipher_page_size = 4096;")?;
    Ok(())
}

/// The §9.1 sequence, in the order the order matters.
fn bootstrap(conn: &Connection, key: Option<&DbKey>) -> Result<()> {
    if let Some(key) = key {
        apply_key(conn, key)?;
    }

    // auto_vacuum comes BEFORE journal_mode, which is a deviation from the
    // order as written in §9.1, and it is deliberate. SQLite only lets
    // auto_vacuum move off NONE while the database is still uninitialised;
    // once anything fixes the page size the setting is frozen and only a full
    // VACUUM can change it. `PRAGMA journal_mode = WAL` writes the header, so
    // running it first on a brand-new file would close that window forever.
    // On an existing file this pragma is a harmless no-op, since the value is
    // already baked into the header.
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\
         PRAGMA foreign_keys = ON;\
         PRAGMA synchronous = NORMAL;\
         PRAGMA busy_timeout = 5000;\
         PRAGMA secure_delete = ON;\
         PRAGMA temp_store = MEMORY;",
    )?;

    // Force a read of page 1. Until something actually decrypts a page,
    // SQLCipher has no idea whether the key is right, so a wrong key would
    // otherwise sail through open() and only fail at the first query — by
    // which time the caller has lost the context to say "wrong key" rather
    // than "database corrupt".
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
        r.get::<_, i64>(0)
    })?;

    Ok(())
}

/// Overwrite a string's bytes in place before it is dropped.
///
/// The `PRAGMA key` statement is the one place the raw key exists as text.
/// `write_volatile` is what keeps the optimiser from removing stores to memory
/// it can prove is never read again (docs/REQUIREMENTS.md 10).
fn zeroize(s: &mut String) {
    // SAFETY: every byte is overwritten with 0x00, which is valid UTF-8, so
    // the `String` invariant holds for the whole window in which it is
    // observable.
    let bytes = unsafe { s.as_mut_vec() };
    for b in bytes.iter_mut() {
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    bytes.clear();
}

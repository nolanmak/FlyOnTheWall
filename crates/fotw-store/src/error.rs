//! The one error type this crate hands out.

use std::path::PathBuf;

/// Everything that can go wrong opening or driving the store.
///
/// Deliberately not a transparent re-export of [`rusqlite::Error`]: the two
/// failures a *user* can actually act on — a wrong key and a library that is
/// too old for their database — must be distinguishable from "SQLite said no",
/// because the UI copy for them is completely different.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The database was written by a newer build of FlyOnTheWall.
    ///
    /// This is a refusal, never a downgrade. Migrations are forward-only, so a
    /// binary that opened a `user_version` it does not understand would run
    /// this-version code against next-version tables and corrupt the library
    /// in ways no backup taken afterwards can undo.
    #[error(
        "this library was written by a newer version of FlyOnTheWall \
         (schema v{found}, this build understands up to v{supported}); \
         upgrade the app to open it"
    )]
    DatabaseTooNew {
        /// `PRAGMA user_version` found in the file.
        found: usize,
        /// The highest migration this binary carries.
        supported: usize,
    },

    /// A migration failed. The pre-migration backup (see
    /// [`crate::Db::open`]) is the recovery path.
    #[error("migration failed: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    /// SQLite/SQLCipher rejected an operation. A wrong `PRAGMA key` surfaces
    /// here as `file is not a database`.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The key material was not 32 bytes.
    #[error("invalid database key: {0}")]
    InvalidKey(String),

    /// A row was requested by id and does not exist.
    #[error("no such {kind}: {id}")]
    NotFound {
        /// The table, in human words (`meeting`, `transcript`, ...).
        kind: &'static str,
        /// The id that was looked up.
        id: String,
    },

    /// A caller-supplied value was rejected before it reached SQLite.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Filesystem failure, with the path that caused it.
    #[error("io error while {context} ({path})")]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

impl StoreError {
    /// Attach a path and an operation to an [`std::io::Error`].
    pub(crate) fn io(
        context: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            context,
            path: path.into(),
            source,
        }
    }

    /// True when the fix is "install a newer build", not "retry".
    #[must_use]
    pub fn is_database_too_new(&self) -> bool {
        matches!(self, Self::DatabaseTooNew { .. })
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, StoreError>;

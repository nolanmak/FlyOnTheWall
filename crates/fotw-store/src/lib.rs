//! `fotw-store` — the SQLCipher-encrypted library every meeting lives in.
//!
//! See docs/REQUIREMENTS.md 9. The shape of this crate follows three rules
//! from that section, each of which is a rule because the alternative fails
//! quietly rather than loudly:
//!
//! 1. **[`Db::open`] is the only way to get a connection.** The bootstrap
//!    pragmas in §9.1 are all silent-on-omission: a connection that skipped
//!    `PRAGMA foreign_keys` still works, it just stops cascading deletes.
//! 2. **Nothing here is mutated in place that two devices could both rewrite**
//!    (§9.7). Summaries are versioned rows, transcripts accumulate rather than
//!    replace, and deletes leave tombstones. Sync is a non-goal; a schema that
//!    forecloses it is not.
//! 3. **Deleting means the bytes are gone** (§9.6). [`Db::delete_meeting`] is
//!    one transaction plus a vacuum and a checkpoint, and its acceptance test
//!    greps the raw file for the transcript text. Since migration 0002 that
//!    clause covers the FTS5 indexes too: an external-content index keeps the
//!    tokens after the source row is deleted unless a trigger retracts them.
//!
//! ```no_run
//! use fotw_store::{Db, DbKey, NewMeeting};
//!
//! # fn main() -> Result<(), fotw_store::StoreError> {
//! let key = DbKey::from_bytes([0u8; 32]); // real ones come from the keychain
//! let mut db = Db::open("/tmp/fotw/db.sqlite3", &key)?;
//!
//! let id = db.meetings().create(NewMeeting::new("device-1", "Europe/Berlin"))?;
//! let meeting = db.meetings().get(&id)?;
//! assert_eq!(meeting.state, "recording");
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod archive;
mod db;
mod delete;
mod error;
pub mod export;
mod ids;
mod key;
mod migrations;
mod models;
mod repo;
mod search;

pub use crate::archive::{
    ArchiveOptions, ArchiveReport, Conflict, ImportReport, LIBRARY_SCHEMA, LibraryManifest,
    PLAINTEXT_WARNING, Progress, Projection, archived_tables,
};
pub use crate::db::Db;
pub use crate::delete::DeleteOutcome;
pub use crate::error::{Result, StoreError};
pub use crate::export::{Blob, Clipboard, MEETING_SCHEMA, MeetingDoc};
pub use crate::ids::{new_id, now_ms};
pub use crate::key::DbKey;
pub use crate::migrations::LATEST_SCHEMA_VERSION;
pub use crate::models::{Meeting, NewMeeting, NewSegment, NewSummary, NoteAnchor, Summary};
pub use crate::repo::MeetingRepo;
pub use crate::search::{FTS_TABLES, SearchHit, SearchQuery, SearchSource, SearchWeights};

//! The real [`MeetingSource`]: `fotw_store` behind a mutex.
//!
//! Nothing here reimplements a query. `Db::search`, `MeetingRepo::list`,
//! `::get`, `::transcript_text` and `::current_summary` already exist, are
//! already tested against a 1,250-meeting corpus, and already carry the
//! reasoning about why search is two statements and not one (§9.4). A second
//! implementation of any of them here would be a second thing to keep correct.
//!
//! # Why a `Mutex<Db>` and not a pool
//!
//! `rusqlite::Connection` is `Send` but not `Sync`, and §9.1 makes `Db::open`
//! the only way to get one because the bootstrap pragmas are all
//! silent-on-omission. A connection pool would mean opening more connections,
//! each of which has to redo `PRAGMA key`, `foreign_keys`, `journal_mode` and
//! the rest — and the one that skipped a pragma still works, it just stops
//! cascading deletes. One connection behind a mutex is the boring option, and
//! the load here is a single local browser.
//!
//! Every call is made from [`tokio::task::spawn_blocking`] (see
//! [`crate::api`]), so holding the mutex across a query blocks a blocking-pool
//! thread rather than a runtime worker.

use std::sync::Mutex;

use fotw_store::{Db, SearchQuery, StoreError};

use crate::source::{Hit, MeetingDetail, MeetingRow, MeetingSource, Segment, SourceError};

/// A [`MeetingSource`] over an open, unlocked [`Db`].
#[derive(Debug)]
pub struct StoreSource {
    db: Mutex<Db>,
}

impl StoreSource {
    /// Wrap an open database.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db: Mutex::new(db) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Db> {
        // A panic inside a query leaves SQLite consistent — the transaction is
        // rolled back by `rusqlite`'s guard — so the poison flag carries no
        // information here, and honouring it would take the UI down for the
        // rest of the session.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl MeetingSource for StoreSource {
    fn list(&self, limit: u32, offset: u32) -> Result<Vec<MeetingRow>, SourceError> {
        let mut db = self.lock();
        let meetings = db
            .meetings()
            .list(i64::from(limit), i64::from(offset))
            .map_err(map_err)?;
        Ok(meetings.into_iter().map(row).collect())
    }

    fn detail(&self, id: &str) -> Result<Option<MeetingDetail>, SourceError> {
        let mut db = self.lock();
        let meeting = match db.meetings().get(id) {
            Ok(m) => m,
            // "No such meeting" is not an error to the API; it is a 404 that
            // looks exactly like every other 404 (ING-09).
            Err(StoreError::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(map_err(e)),
        };

        let summary_md = db
            .meetings()
            .current_summary(id)
            .map_err(map_err)?
            .map(|s| s.body_md);

        let segments = match primary_transcript_id(&db, id) {
            Some(transcript_id) => db
                .meetings()
                .transcript_text(&transcript_id)
                .map_err(map_err)?
                .into_iter()
                .map(|(idx, text)| Segment { idx, text })
                .collect(),
            // A meeting with no transcript is normal, not broken: recording
            // without a provider configured is a supported state.
            None => Vec::new(),
        };

        Ok(Some(MeetingDetail {
            meeting: row(meeting),
            summary_md,
            segments,
        }))
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<Hit>, SourceError> {
        let db = self.lock();
        let hits = db
            .search(&SearchQuery::new(query).limit(i64::from(limit)))
            .map_err(map_err)?;
        Ok(hits
            .into_iter()
            .map(|h| Hit {
                meeting_id: h.meeting_id,
                meeting_title: h.meeting_title,
                started_at_ms: h.started_at_ms,
                source: h.source.as_str().to_owned(),
                start_ms: h.start_ms,
                snippet: h.snippet,
            })
            .collect())
    }
}

/// The primary transcript of a meeting, if it has one.
///
/// `MeetingRepo` has no accessor for this — `transcript_text` takes a
/// transcript id — so the query lives here, spelled the same way `fotwd`'s
/// `persist::primary_transcript_id` already spells it.
///
/// **Known wart, recorded rather than hidden:** `.ok()` folds a genuine SQLite
/// failure into "this meeting has no transcript", so a broken database renders
/// as an empty transcript rather than as an error. Naming
/// `rusqlite::Error::QueryReturnedNoRows` to tell the two apart would make
/// `fotw-web` depend on `rusqlite` directly, which is exactly the coupling
/// [`MeetingSource`] exists to prevent. The right fix is a
/// `MeetingRepo::primary_transcript_id` in `fotw-store`, which would also
/// delete the duplicate in `fotwd`.
fn primary_transcript_id(db: &Db, meeting_id: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT id FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1",
            [meeting_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
}

fn row(m: fotw_store::Meeting) -> MeetingRow {
    MeetingRow {
        id: m.id,
        title: m.title,
        started_at_ms: m.started_at_ms,
        duration_ms: m.duration_ms,
        state: m.state,
    }
}

/// `InvalidArgument` is the store's "that is not a query" — an empty `MATCH`
/// is a syntax error in FTS5 and §9.4 refuses rather than guessing. It is the
/// user's input, so it becomes [`SourceError::BadQuery`] and the search box
/// shows no results; everything else is a fault and becomes a bare 500.
fn map_err(e: StoreError) -> SourceError {
    match e {
        StoreError::InvalidArgument(m) => SourceError::BadQuery(m),
        other => SourceError::Backend(other.to_string()),
    }
}

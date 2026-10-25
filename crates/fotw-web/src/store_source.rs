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
    fn note(&self, id: &str) -> Result<crate::notes::NoteDocument, crate::notes::NoteError> {
        self.lock()
            .note_document(id)
            .map(wire_note)
            .map_err(note_error)
    }

    fn save_note(
        &self,
        id: &str,
        doc: crate::notes::NoteDocument,
    ) -> Result<crate::notes::NoteDocument, crate::notes::NoteError> {
        let mut doc = stored_note(doc);
        doc.revision = self
            .lock()
            .save_note_document(id, doc.revision, &doc.body_md, &doc.anchors)
            .map_err(note_error)?;
        Ok(wire_note(doc))
    }

    fn note_drafts(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::notes::DraftSummary>, crate::notes::NoteError> {
        self.lock()
            .session_note_drafts(limit, offset)
            .map_err(note_error)
            .map(|rows| {
                rows.into_iter()
                    .map(|d| crate::notes::DraftSummary {
                        session_id: d.session_id,
                        started_at_ms: d.started_at_ms,
                        updated_at_ms: d.updated_at_ms,
                    })
                    .collect()
            })
    }

    fn session_note(
        &self,
        id: &str,
        clock: Option<crate::notes::SessionClock>,
    ) -> Result<crate::notes::SessionNote, crate::notes::NoteError> {
        let mut db = self.lock();
        let draft = db.session_note(id).map_err(note_error)?;
        let frozen = db.session_note_clock(id).map_err(note_error)?;
        let clock = frozen
            .map(|(started_at_ms, elapsed_ms)| crate::notes::SessionClock {
                started_at_ms,
                elapsed_ms,
            })
            .or(clock);
        let (started, note) = match draft {
            Some(d) => (d.started_at_ms, wire_note(d.note)),
            None => (
                clock
                    .ok_or(crate::notes::NoteError::NotFound)?
                    .started_at_ms,
                crate::notes::NoteDocument {
                    revision: 0,
                    body_md: String::new(),
                    anchors: vec![],
                },
            ),
        };
        if clock.is_some_and(|c| c.started_at_ms != started) {
            return Err(crate::notes::NoteError::Invalid);
        }
        Ok(crate::notes::SessionNote {
            session_id: id.into(),
            meeting_id: db.session_note_meeting(id).map_err(note_error)?,
            started_at_ms: started,
            elapsed_ms: clock.map(|c| c.elapsed_ms),
            note,
        })
    }

    fn save_session_note(
        &self,
        id: &str,
        doc: crate::notes::NoteDocument,
        clock: Option<crate::notes::SessionClock>,
    ) -> Result<crate::notes::SessionNote, crate::notes::NoteError> {
        let mut db = self.lock();
        let frozen = db.session_note_clock(id).map_err(note_error)?;
        let clock = frozen
            .map(|(started_at_ms, elapsed_ms)| crate::notes::SessionClock {
                started_at_ms,
                elapsed_ms,
            })
            .or(clock);
        let Some(clock) = clock else {
            return Err(if db.session_note(id).map_err(note_error)?.is_some() {
                crate::notes::NoteError::Unavailable
            } else {
                crate::notes::NoteError::NotFound
            });
        };
        let mut draft = fotw_store::SessionNoteDocument {
            session_id: id.into(),
            started_at_ms: clock.started_at_ms,
            note: stored_note(doc),
        };
        draft.note.revision = db
            .save_session_note(&draft, clock.elapsed_ms)
            .map_err(note_error)?;
        Ok(crate::notes::SessionNote {
            session_id: id.into(),
            meeting_id: db.session_note_meeting(id).map_err(note_error)?,
            started_at_ms: clock.started_at_ms,
            elapsed_ms: Some(clock.elapsed_ms),
            note: wire_note(draft.note),
        })
    }

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

        // Propagates a real database failure instead of folding it into "no
        // transcript". `MeetingRepo::primary_transcript_id` distinguishes the
        // two; the previous hand-rolled query used `.ok()` and would have
        // rendered a broken library as an empty meeting.
        // The anchors are dropped here and kept in the store: they exist to
        // re-anchor a block after an edit, which is the editor's problem, not
        // the reader's.
        let note_md = db
            .meetings()
            .note(id)
            .map_err(map_err)?
            .map(|(body, _)| body);

        let segments = match db.meetings().primary_transcript_id(id).map_err(map_err)? {
            Some(transcript_id) => db
                .meetings()
                .transcript_segments(&transcript_id)
                .map_err(map_err)?
                .into_iter()
                .map(|s| Segment {
                    idx: s.idx,
                    start_ms: s.start_ms,
                    channel: s.channel,
                    speaker: s.speaker,
                    text: s.text,
                })
                .collect(),
            // A meeting with no transcript is normal, not broken: recording
            // without a provider configured is a supported state.
            None => Vec::new(),
        };

        let speech_provenance = db
            .speech_provenance(id)
            .map_err(map_err)?
            .into_iter()
            .map(|epoch| crate::source::SavedSpeechEpoch {
                source: epoch.source,
                epoch: epoch.epoch,
                from_sample: epoch.from_sample,
                through_sample: epoch.through_sample,
                provider: epoch.provider,
                model: epoch.model,
                revision: epoch.revision,
                capabilities: epoch.capabilities,
                reason: epoch.reason,
            })
            .collect();
        Ok(Some(MeetingDetail {
            speech_provenance,
            enrich_status: meeting.enrich_status.clone(),
            enrich_detail: meeting.enrich_detail.clone(),
            meeting: row(meeting),
            summary_md,
            note_md,
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

fn note_error(error: StoreError) -> crate::notes::NoteError {
    use crate::notes::NoteError;
    match error {
        StoreError::NotFound { .. } => NoteError::NotFound,
        StoreError::NoteConflict => NoteError::Conflict,
        StoreError::InvalidArgument(_) => NoteError::Invalid,
        _ => NoteError::Backend,
    }
}

fn wire_note(doc: fotw_store::NoteDocument) -> crate::notes::NoteDocument {
    crate::notes::NoteDocument {
        revision: doc.revision,
        body_md: doc.body_md,
        anchors: doc
            .anchors
            .into_iter()
            .map(|a| crate::notes::NoteAnchor {
                id: Some(a.id),
                block_idx: a.block_idx,
                block_text: a.block_text,
                typed_at_ms: a.typed_at_ms,
            })
            .collect(),
    }
}

fn stored_note(doc: crate::notes::NoteDocument) -> fotw_store::NoteDocument {
    fotw_store::NoteDocument {
        revision: doc.revision,
        body_md: doc.body_md,
        anchors: doc
            .anchors
            .into_iter()
            .map(|a| {
                let mut anchor =
                    fotw_store::NoteAnchor::new(a.block_idx, a.block_text, a.typed_at_ms);
                if let Some(id) = a.id {
                    anchor.id = id;
                }
                anchor
            })
            .collect(),
    }
}

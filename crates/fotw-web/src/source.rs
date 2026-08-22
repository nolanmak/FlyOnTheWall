//! What the API is allowed to know about the library.
//!
//! The HTTP layer talks to a [`MeetingSource`], not to `fotw_store::Db`. Two
//! reasons, in order of importance:
//!
//! 1. **The security tests must not need a database.** Every test in
//!    `tests/ingress.rs` and `tests/stream.rs` is about headers, tokens and
//!    origins; none of them is about SQL. Wiring them to the real store would
//!    make each one open a SQLCipher file, which means a key, a temp dir and
//!    ~90 s of vendored-OpenSSL compile before the first assertion runs — and
//!    a slow security suite is a security suite people stop running.
//! 2. **The wire format is decided here, not by the schema.** The DTOs below
//!    are what leaves the process. If the API serialised
//!    `fotw_store::Meeting` directly, adding a column to the table would
//!    silently add a field to the API — and `credentials`-adjacent columns are
//!    one careless join away from that.
//!
//! The real adapter is [`crate::store_source`], behind the `store` feature.

use serde::{Deserialize, Serialize};

/// Anything that can answer the four questions the UI asks.
///
/// `Send + Sync + 'static` because handlers hand it to
/// [`tokio::task::spawn_blocking`]: the real implementation is `rusqlite`,
/// which blocks, and blocking a runtime worker while a two-hour transcript is
/// read would stall every other connection — including the live delta stream.
pub trait MeetingSource: Send + Sync + 'static {
    /// Most recent meetings first.
    ///
    /// # Errors
    ///
    /// Whatever the backing store failed with.
    fn list(&self, limit: u32, offset: u32) -> Result<Vec<MeetingRow>, SourceError>;

    /// One meeting with its transcript and current summary, or `None` if there
    /// is no such meeting.
    ///
    /// # Errors
    ///
    /// Whatever the backing store failed with.
    fn detail(&self, id: &str) -> Result<Option<MeetingDetail>, SourceError>;

    /// Full-text search across titles, notes, summaries and transcripts.
    ///
    /// # Errors
    ///
    /// Whatever the backing store failed with, including a query with no
    /// searchable token in it.
    fn search(&self, query: &str, limit: u32) -> Result<Vec<Hit>, SourceError>;
}

/// A failure inside the store, as far as the web layer cares.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The query was not one the store could run — an empty search, say.
    ///
    /// Kept distinct from [`SourceError::Backend`] because it is the user's
    /// input rather than a fault, and it is the only one the UI can act on.
    #[error("{0}")]
    BadQuery(String),
    /// Anything else.
    ///
    /// The message never reaches the client: §10's never-log rules apply to
    /// meeting content, and a SQLite error string can quote the row it choked
    /// on. Handlers map this to a bare 500 with no body.
    #[error("{0}")]
    Backend(String),
}

/// A meeting as the list view needs it.
///
/// Deliberately not `fotw_store::Meeting`: no `lamport`, no
/// `origin_device_id`, no `template_id`. Sync bookkeeping is not the browser's
/// business and shipping it would make it API surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingRow {
    /// UUIDv7 primary key.
    pub id: String,
    /// The meeting's title.
    pub title: String,
    /// Milliseconds since the Unix epoch.
    pub started_at_ms: i64,
    /// Wall-clock length, once the meeting has ended.
    pub duration_ms: Option<i64>,
    /// `recording`, `transcribing` or `ready`.
    pub state: String,
}

/// One meeting, opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingDetail {
    /// The same header the list view showed.
    pub meeting: MeetingRow,
    /// The current summary's markdown body, if one has been generated.
    pub summary_md: Option<String>,
    /// The user's own notes, if they typed any.
    ///
    /// Search has always indexed notes, so a user could match their own note,
    /// land on the meeting, and find it nowhere on the page. In a product whose
    /// premise is "you write, we augment", the note is not a secondary field.
    pub note_md: Option<String>,
    /// The primary transcript, in order.
    pub segments: Vec<Segment>,
}

/// One transcript segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// Position in the transcript.
    pub idx: i64,
    /// Milliseconds from the start of the meeting.
    pub start_ms: i64,
    /// `mic` or `system` — which capture leg produced these words.
    ///
    /// The user's own voice versus everybody else's. It survives capture,
    /// transcription and storage; dropping it here is what made a stored
    /// transcript render both legs identically (#64).
    pub channel: String,
    /// Diarisation label, e.g. `S0`, when the provider diarised.
    ///
    /// **Also attacker-influenced**, for a subtler reason than the text: the
    /// label is a provider string, and once speaker *naming* lands it becomes
    /// a user string. It goes to the DOM the same way the text does.
    pub speaker: Option<String>,
    /// The words.
    ///
    /// **Attacker-influenced.** A participant can say anything, so this string
    /// reaches the DOM through `textContent` and never `innerHTML`, and the
    /// CSP in [`crate::assets`] is the second line of that defence (ING-11).
    pub text: String,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    /// The meeting the hit is in.
    pub meeting_id: String,
    /// Its title, so the result list needs no second round trip.
    pub meeting_title: String,
    /// When the meeting started.
    pub started_at_ms: i64,
    /// Which index matched: `title`, `note`, `summary` or `transcript`.
    pub source: String,
    /// Offset into the meeting for a transcript hit.
    pub start_ms: Option<i64>,
    /// The matched passage, with terms wrapped in `[` and `]`.
    pub snippet: String,
}

/// An in-memory [`MeetingSource`].
///
/// Public rather than `#[cfg(test)]`: the daemon's own integration tests want
/// it too, and a fake that only exists inside this crate's `cfg(test)` gets
/// reimplemented — differently — by every other crate that needs one.
#[derive(Debug, Default)]
pub struct MemorySource {
    meetings: Vec<MeetingDetail>,
}

impl MemorySource {
    /// An empty library.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a meeting. Later meetings should have later `started_at_ms`; `list`
    /// sorts, so the caller does not have to.
    #[must_use]
    pub fn with_meeting(mut self, detail: MeetingDetail) -> Self {
        self.meetings.push(detail);
        self
    }
}

impl MeetingSource for MemorySource {
    fn list(&self, limit: u32, offset: u32) -> Result<Vec<MeetingRow>, SourceError> {
        let mut rows: Vec<MeetingRow> = self.meetings.iter().map(|m| m.meeting.clone()).collect();
        rows.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms).then(b.id.cmp(&a.id)));
        Ok(rows
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect())
    }

    fn detail(&self, id: &str) -> Result<Option<MeetingDetail>, SourceError> {
        Ok(self.meetings.iter().find(|m| m.meeting.id == id).cloned())
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<Hit>, SourceError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Err(SourceError::BadQuery("empty query".into()));
        }
        let mut hits = Vec::new();
        for m in &self.meetings {
            for seg in &m.segments {
                if seg.text.to_lowercase().contains(&needle) {
                    hits.push(Hit {
                        meeting_id: m.meeting.id.clone(),
                        meeting_title: m.meeting.title.clone(),
                        started_at_ms: m.meeting.started_at_ms,
                        source: "transcript".into(),
                        start_ms: Some(seg.idx * 1000),
                        snippet: seg.text.clone(),
                    });
                }
            }
        }
        hits.truncate(limit as usize);
        Ok(hits)
    }
}

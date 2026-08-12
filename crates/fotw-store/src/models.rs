//! The typed row shapes the repository speaks in.
//!
//! Insert types and read types are deliberately separate. An insert type has
//! no `id`, no `created_at` and no `lamport`: those are the store's to mint,
//! and a caller that could set them could also set them wrong — which for
//! `lamport` means silently losing a future merge (docs/REQUIREMENTS.md 9.7
//! invariant 2).

use rusqlite::Row;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A meeting as it exists in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    /// UUIDv7.
    pub id: String,
    /// User-visible title; empty until the calendar or the user supplies one.
    pub title: String,
    /// Epoch milliseconds UTC.
    pub started_at_ms: i64,
    /// Epoch milliseconds UTC, `None` while still recording.
    pub ended_at_ms: Option<i64>,
    /// Wall-clock length, denormalised so the list view does not have to
    /// subtract nullable columns for every row.
    pub duration_ms: Option<i64>,
    /// IANA timezone name for display, e.g. `Europe/Berlin`.
    pub tz: String,
    /// Owning folder, if any.
    pub folder_id: Option<String>,
    /// Summary template to use, if any.
    pub template_id: Option<String>,
    /// `recording` | `transcribing` | `ready` | `failed`.
    pub state: String,
    /// Detected or forced transcription language.
    pub language: Option<String>,
    /// Whether the participants were told they were being recorded (§11).
    pub disclosed: bool,
    /// `default` | `forever` | `until_transcribed` | `days`.
    pub retain_audio: String,
    /// Only meaningful when `retain_audio == "days"`.
    pub retain_audio_days: Option<i64>,
    /// Epoch milliseconds UTC.
    pub created_at: i64,
    /// Epoch milliseconds UTC.
    pub updated_at: i64,
    /// Merge counter; ordered against `origin_device_id`, never wall clock.
    pub lamport: i64,
    /// The device that created this row.
    pub origin_device_id: String,
}

impl Meeting {
    pub(crate) const COLUMNS: &'static str = "id, title, started_at_ms, ended_at_ms, duration_ms, \
         tz, folder_id, template_id, state, language, disclosed, retain_audio, \
         retain_audio_days, created_at, updated_at, lamport, origin_device_id";

    pub(crate) fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            title: row.get(1)?,
            started_at_ms: row.get(2)?,
            ended_at_ms: row.get(3)?,
            duration_ms: row.get(4)?,
            tz: row.get(5)?,
            folder_id: row.get(6)?,
            template_id: row.get(7)?,
            state: row.get(8)?,
            language: row.get(9)?,
            disclosed: row.get::<_, i64>(10)? != 0,
            retain_audio: row.get(11)?,
            retain_audio_days: row.get(12)?,
            created_at: row.get(13)?,
            updated_at: row.get(14)?,
            lamport: row.get(15)?,
            origin_device_id: row.get(16)?,
        })
    }
}

/// What a caller supplies to start a meeting.
///
/// `tz` and `origin_device_id` are required positionally because there is no
/// defensible default for either: a missing timezone renders every timestamp
/// in the wrong place, and a missing device id breaks merge ordering.
#[derive(Debug, Clone)]
pub struct NewMeeting {
    /// The device recording this meeting.
    pub origin_device_id: String,
    /// IANA timezone name for display.
    pub tz: String,
    /// Title; may be empty and filled in later.
    pub title: String,
    /// Epoch milliseconds UTC. Defaults to now.
    pub started_at_ms: i64,
    /// Initial state; defaults to `recording`.
    pub state: String,
    /// Owning folder.
    pub folder_id: Option<String>,
    /// Summary template.
    pub template_id: Option<String>,
    /// Calendar event UID this was matched to.
    pub calendar_uid: Option<String>,
    /// Which calendar it came from.
    pub calendar_source: Option<String>,
    /// Conference URL, if one was detected.
    pub meeting_url: Option<String>,
    /// The app that appeared to be hosting the call.
    pub app_hint: Option<String>,
    /// Forced language, if the user picked one.
    pub language: Option<String>,
    /// Whether participants were told (§11).
    pub disclosed: bool,
}

impl NewMeeting {
    /// A meeting starting now, in `recording` state.
    pub fn new(origin_device_id: impl Into<String>, tz: impl Into<String>) -> Self {
        Self {
            origin_device_id: origin_device_id.into(),
            tz: tz.into(),
            title: String::new(),
            started_at_ms: crate::ids::now_ms(),
            state: "recording".to_owned(),
            folder_id: None,
            template_id: None,
            calendar_uid: None,
            calendar_source: None,
            meeting_url: None,
            app_hint: None,
            language: None,
            disclosed: false,
        }
    }

    /// Set the title.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Override the start time (epoch ms UTC).
    #[must_use]
    pub fn started_at_ms(mut self, ms: i64) -> Self {
        self.started_at_ms = ms;
        self
    }

    /// Override the initial state.
    #[must_use]
    pub fn state(mut self, state: impl Into<String>) -> Self {
        self.state = state.into();
        self
    }

    /// Record that participants were told about the recording.
    #[must_use]
    pub fn disclosed(mut self, disclosed: bool) -> Self {
        self.disclosed = disclosed;
        self
    }
}

/// One transcript segment, as the STT layer produces them.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSegment {
    /// Position within the transcript. Unique per transcript, and the reason
    /// re-running a provider cannot interleave with an existing run.
    pub idx: i64,
    /// Milliseconds from meeting start.
    pub start_ms: i64,
    /// Milliseconds from meeting start.
    pub end_ms: i64,
    /// `mic` or `system`. Kept per-segment because the two legs are never
    /// mixed before transcription (§7.5).
    pub channel: String,
    /// Diarisation label as the provider gave it.
    pub speaker_label: Option<String>,
    /// The person this label has been bound to, once known.
    pub person_id: Option<String>,
    /// The transcribed text.
    pub text: String,
    /// Provider confidence, 0..1.
    pub confidence: Option<f64>,
    /// False for an interim hypothesis that may still be revised.
    pub is_final: bool,
    /// zstd'd JSON word timings. A BLOB rather than rows: ~8.4M words a year
    /// for a heavy user, and no query ever wants one word (§9.3).
    pub words: Option<Vec<u8>>,
}

impl NewSegment {
    /// A final `mic` segment with no diarisation and no word timings.
    pub fn new(idx: i64, start_ms: i64, end_ms: i64, text: impl Into<String>) -> Self {
        Self {
            idx,
            start_ms,
            end_ms,
            channel: "mic".to_owned(),
            speaker_label: None,
            person_id: None,
            text: text.into(),
            confidence: None,
            is_final: true,
            words: None,
        }
    }

    /// Set the channel (`mic` or `system`).
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = channel.into();
        self
    }

    /// Attach compressed word timings.
    #[must_use]
    pub fn words(mut self, words: Vec<u8>) -> Self {
        self.words = Some(words);
        self
    }
}

/// One transcript segment, as stored.
///
/// Distinct from `fotw_stt::TranscriptSegment`, which carries how the text was
/// *obtained* — provider, model, revision, timestamp source. Those describe a
/// live streaming session and are meaningless once the row is on disk. This is
/// what a reader needs.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredSegment {
    /// Position in the transcript.
    pub idx: i64,
    /// Milliseconds from the start of the meeting.
    pub start_ms: i64,
    /// Milliseconds from the start of the meeting.
    pub end_ms: i64,
    /// Diarisation label, e.g. `S0`. `None` when the provider did not
    /// diarise, which is a normal state and not a fault.
    pub speaker: Option<String>,
    /// The words. **Attacker-influenced**: anyone in the meeting chooses this.
    pub text: String,
    /// Provider confidence, when reported.
    pub confidence: Option<f64>,
}

/// One markdown block of the user's notes, and when they started typing it.
///
/// `typed_at_ms` is relative to the meeting start, not the epoch: the meeting's
/// start time can be corrected after the fact, and the alignment must survive
/// that.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteAnchor {
    /// Position of the block in the document.
    pub block_idx: i64,
    /// Snapshot of the block text, used to re-anchor after later edits.
    pub block_text: String,
    /// Milliseconds from meeting start, first keystroke in this block.
    pub typed_at_ms: i64,
}

impl NoteAnchor {
    /// Build an anchor.
    pub fn new(block_idx: i64, block_text: impl Into<String>, typed_at_ms: i64) -> Self {
        Self {
            block_idx,
            block_text: block_text.into(),
            typed_at_ms,
        }
    }
}

/// A generated summary, before the store assigns it a version.
#[derive(Debug, Clone)]
pub struct NewSummary {
    /// The device that generated it.
    pub origin_device_id: String,
    /// LLM provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// sha256 of the fully rendered prompt, for reproducibility (§8.3).
    pub prompt_hash: String,
    /// The summary body.
    pub body_md: String,
    /// Template used, if any.
    pub template_id: Option<String>,
    /// Transcript it was generated from, if known.
    pub transcript_id: Option<String>,
    /// Citation coverage from §8.6, if measured.
    pub coverage: Option<f64>,
    /// Billing telemetry.
    pub input_tokens: Option<i64>,
    /// Billing telemetry.
    pub output_tokens: Option<i64>,
    /// Billing telemetry.
    pub cost_micros: Option<i64>,
}

impl NewSummary {
    /// A summary with only the fields that have no sensible default.
    pub fn new(
        origin_device_id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        prompt_hash: impl Into<String>,
        body_md: impl Into<String>,
    ) -> Self {
        Self {
            origin_device_id: origin_device_id.into(),
            provider: provider.into(),
            model: model.into(),
            prompt_hash: prompt_hash.into(),
            body_md: body_md.into(),
            template_id: None,
            transcript_id: None,
            coverage: None,
            input_tokens: None,
            output_tokens: None,
            cost_micros: None,
        }
    }

    /// Link the transcript this was generated from.
    #[must_use]
    pub fn transcript_id(mut self, id: impl Into<String>) -> Self {
        self.transcript_id = Some(id.into());
        self
    }

    /// Record the §8.6 citation-coverage metric.
    #[must_use]
    pub fn coverage(mut self, coverage: f64) -> Self {
        self.coverage = Some(coverage);
        self
    }
}

/// A stored summary version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// UUIDv7.
    pub id: String,
    /// Owning meeting.
    pub meeting_id: String,
    /// 1-based, monotonically increasing per meeting.
    pub version: i64,
    /// Template used, if any.
    pub template_id: Option<String>,
    /// Transcript it was generated from, if known.
    pub transcript_id: Option<String>,
    /// LLM provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// sha256 of the rendered prompt.
    pub prompt_hash: String,
    /// The summary body.
    pub body_md: String,
    /// Citation coverage from §8.6.
    pub coverage: Option<f64>,
    /// Whether this is the version the UI shows.
    pub is_current: bool,
    /// Epoch milliseconds UTC.
    pub created_at: i64,
    /// The device that generated it.
    pub origin_device_id: String,
}

impl Summary {
    pub(crate) const COLUMNS: &'static str = "id, meeting_id, version, template_id, transcript_id, provider, model, \
         prompt_hash, body_md, coverage, is_current, created_at, origin_device_id";

    pub(crate) fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            meeting_id: row.get(1)?,
            version: row.get(2)?,
            template_id: row.get(3)?,
            transcript_id: row.get(4)?,
            provider: row.get(5)?,
            model: row.get(6)?,
            prompt_hash: row.get(7)?,
            body_md: row.get(8)?,
            coverage: row.get(9)?,
            is_current: row.get::<_, i64>(10)? != 0,
            created_at: row.get(11)?,
            origin_device_id: row.get(12)?,
        })
    }
}

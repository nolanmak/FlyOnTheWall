//! Shared contract for attendee release preparation (#326).
//!
//! The preparation steps are built in separate pull requests: the cut, the
//! release summary and the renderer (#329), the independent review (#330), and
//! the worker that drives them (#331). This module fixes the types that cross
//! between them so each can be built and tested against fakes in parallel.
//! Change a type here only together with every implementer.

use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future, the codebase's pattern for async trait methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One source segment, as release reads it (D6). `index` is the segment's
/// position in the meeting's primary transcript, the `[#N]` label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSegment {
    /// Zero-based index in the primary transcript.
    pub index: usize,
    /// Offset from capture start, in milliseconds.
    pub offset_ms: u64,
    /// Recorded speaker label, or "Call audio" for a mic-only meeting.
    pub speaker: String,
    /// Unmodified source text.
    pub text: String,
}

/// A meeting-level spelling correction (`name_corrections_for`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameCorrection {
    /// The text as transcribed.
    pub from: String,
    /// The corrected spelling.
    pub to: String,
}

/// Everything preparation reads for one meeting. Built by the worker (#331).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareInput {
    /// The meeting id.
    pub meeting_id: String,
    /// Frozen raw calendar event title (D9).
    pub event_title: String,
    /// The meeting's IANA timezone, for dates and times (D9).
    pub tz: String,
    /// Capture start, epoch milliseconds.
    pub started_at_ms: i64,
    /// Every segment of the primary transcript, in index order, scoped to the
    /// meeting (#336).
    pub segments: Vec<SourceSegment>,
    /// Meeting-level name corrections.
    pub corrections: Vec<NameCorrection>,
    /// Frozen recipients (D4).
    pub recipients: Vec<ReleaseRecipient>,
}

/// A frozen recipient as preparation and review see it (D4, D8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRecipient {
    /// Lowercased address in an allowed domain.
    pub address: String,
    /// Display name from the calendar, if any.
    pub display_name: Option<String>,
    /// Whether this is the recording user (the transcript's `me`).
    pub is_self: bool,
}

/// The release cut (D6). The retained set is [`Cut::retained`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cut {
    /// Last included segment of the meeting proper, inclusive.
    pub end_index: usize,
    /// Inclusive, ascending ranges of off-topic or private segments.
    pub omit_ranges: Vec<[usize; 2]>,
    /// A reviewer's explicit retained set, replacing the model cut when set.
    pub reviewer_retained: Option<Vec<usize>>,
}

impl Cut {
    /// The retained indices for a transcript of `len` segments, ascending.
    #[must_use]
    pub fn retained(&self, len: usize) -> Vec<usize> {
        if let Some(chosen) = &self.reviewer_retained {
            let mut kept: Vec<usize> = chosen.iter().copied().filter(|i| *i < len).collect();
            kept.sort_unstable();
            kept.dedup();
            return kept;
        }
        (0..len)
            .filter(|i| *i <= self.end_index)
            .filter(|i| !self.omit_ranges.iter().any(|[a, b]| i >= a && i <= b))
            .collect()
    }
}

/// A release summary section (D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    /// "Summary"; must hold at least one bullet.
    Summary,
    /// "Decisions".
    Decisions,
    /// "Action items".
    ActionItems,
}

/// One summary bullet with the retained indices it cites (D7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bullet {
    /// Plain bullet text, without citations.
    pub text: String,
    /// Cited retained indices; at least one.
    pub citations: Vec<usize>,
}

/// The release summary (D7). Bullet identity is `(section, ordinal)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseSummary {
    /// `Summary` bullets.
    pub summary: Vec<Bullet>,
    /// `Decisions` bullets.
    pub decisions: Vec<Bullet>,
    /// `Action items` bullets.
    pub action_items: Vec<Bullet>,
}

/// Where each part of the rendered payload came from (5.4, D8).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexMap {
    /// `(first_line, last_line, source_index)`: zero-based, inclusive line
    /// ranges of the attachment that render one retained segment.
    pub attachment_lines: Vec<(usize, usize, usize)>,
    /// `(section, ordinal, citations)` for each body bullet.
    pub bullets: Vec<(Section, usize, Vec<usize>)>,
}

/// The exact outgoing email (D9). What is reviewed is what is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    /// Rendered subject.
    pub subject: String,
    /// Plain-text body, citations stripped, at most 64 KB.
    pub body: String,
    /// `<YYYY-MM-DD>-<slug>.md`.
    pub attachment_name: String,
    /// The rendered Markdown attachment.
    pub attachment: String,
    /// Provenance for the review and the Outbox.
    pub index_map: IndexMap,
}

/// A review finding category (D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Personal, non-work or after-meeting conversation.
    OffMeeting,
    /// Performance, pay, employment or health of a recipient.
    AboutRecipient,
    /// Pay rates, margins, equity, bonus.
    Compensation,
    /// Legal advice, entity, tax, banking.
    LegalFinancial,
    /// Passwords, codes, keys.
    Credential,
    /// Slurs, demeaning remarks.
    Offensive,
}

/// One review finding (D8, 5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The category.
    pub category: Category,
    /// Retained source indices it concerns.
    pub indices: Vec<usize>,
    /// The body bullet it concerns, if any.
    pub bullet: Option<(Section, usize)>,
    /// One line, at most 200 characters.
    pub reason: String,
}

/// Why a preparation step failed. Codes only (D15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// A model call failed, timed out or returned invalid output after one
    /// repair (`held(model_failed)`).
    ModelFailed,
    /// The rendered body exceeds 64 KB (`held(body_too_large)`).
    BodyTooLarge,
    /// The summary policy refuses (`engine_unavailable` pause, D18).
    PolicyRefused,
    /// Policy settings changed mid-call; retry next pass (D18).
    SettingsChanged,
    /// The meeting's inputs changed mid-call; restart preparation (D18).
    InputsChanged,
}

impl PrepareError {
    /// The stable reason code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ModelFailed => "model_failed",
            Self::BodyTooLarge => "body_too_large",
            Self::PolicyRefused => "engine_unavailable",
            Self::SettingsChanged => "settings_changed",
            Self::InputsChanged => "inputs_changed",
        }
    }
}

/// The four preparation steps, implemented by #329 (cut, summarize, render)
/// and #330 (review), driven by the worker in #331. Tests use fakes.
pub trait Preparer: Send + Sync {
    /// The model cut (D6): `end_index` and `omit_ranges` only.
    fn cut<'a>(&'a self, input: &'a PrepareInput) -> BoxFuture<'a, Result<Cut, PrepareError>>;

    /// The release summary from the retained set only (D7).
    fn summarize<'a>(
        &'a self,
        input: &'a PrepareInput,
        retained: &'a [usize],
    ) -> BoxFuture<'a, Result<ReleaseSummary, PrepareError>>;

    /// Render the exact email (D9, 5.4). Pure; no model.
    ///
    /// # Errors
    ///
    /// [`PrepareError::BodyTooLarge`] over 64 KB.
    fn render(
        &self,
        input: &PrepareInput,
        retained: &[usize],
        summary: &ReleaseSummary,
    ) -> Result<Payload, PrepareError>;

    /// The independent review of the exact payload (D8).
    fn review<'a>(
        &'a self,
        input: &'a PrepareInput,
        retained: &'a [usize],
        payload: &'a Payload,
    ) -> BoxFuture<'a, Result<Vec<Finding>, PrepareError>>;

    /// The engine id (engine kind + configured model name) that prepares, for
    /// decisions and the auto unlock (D11, D19).
    fn engine_id(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_follows_the_model_cut() {
        let cut = Cut {
            end_index: 5,
            omit_ranges: vec![[1, 2]],
            reviewer_retained: None,
        };
        assert_eq!(cut.retained(10), vec![0, 3, 4, 5]);
    }

    #[test]
    fn a_reviewer_set_replaces_the_model_cut() {
        let cut = Cut {
            end_index: 2,
            omit_ranges: vec![[1, 1]],
            reviewer_retained: Some(vec![7, 1, 1, 99]),
        };
        assert_eq!(cut.retained(10), vec![1, 7]);
    }
}

//! Purpose-aware, locally saved sharing drafts. No operation sends a document.
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

/// Longest a user-supplied meeting title may be, in characters. Generous beside
/// a machine title (`fotw_summarize::title::TITLE_BUDGET_BYTES`, 64 bytes): a
/// person renaming a call may want more words than an engine is allowed to mint,
/// but not a paragraph in the left menu. The daemon normalises within this.
pub const MAX_TITLE_CHARS: usize = 100;

/// A source excerpt, copied verbatim by code rather than rewritten by a model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentExcerpt {
    /// Original ordered segment index.
    pub index: usize,
    /// Offset from capture start, in milliseconds.
    pub offset_ms: u64,
    /// Absolute capture time, for timezone-aware exports.
    pub at_ms: i64,
    /// A recorded label; a mic-only meeting uses "Call audio".
    pub speaker: String,
    /// Unmodified source text.
    pub text: String,
}

/// An explicit, meeting-scoped spelling correction supplied by the user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NameCorrection {
    /// Name as transcribed.
    pub from: String,
    /// User-confirmed spelling.
    pub to: String,
}

/// Replace whole names without cascading replacements or matching inside words.
#[must_use]
pub fn corrected_names(text: &str, corrections: &[NameCorrection]) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at < text.len() {
        let remaining = &text[at..];
        let boundary_before = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let found = corrections
            .iter()
            .filter(|rule| {
                !rule.from.is_empty()
                    && boundary_before
                    && remaining.starts_with(&rule.from)
                    && remaining[rule.from.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_alphanumeric())
            })
            .max_by_key(|rule| rule.from.len());
        if let Some(rule) = found {
            out.push_str(&rule.to);
            at += rule.from.len();
        } else {
            let c = remaining.chars().next().expect("nonempty suffix");
            out.push(c);
            at += c.len_utf8();
        }
    }
    out
}

/// The editable draft and its selected source material.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharingDocument {
    /// Full source transcript's first speech offset; retained when excerpts are removed.
    /// None on old saved drafts, which are hydrated from their source on load.
    #[serde(default)]
    pub speech_origin_ms: Option<u64>,
    /// Document title. Follows the meeting's own title (the name shown in the
    /// library list and detail header), so the same client's two calls read
    /// differently and a share matches what the left menu calls it. Only a
    /// still-default meeting title falls back to a model-inferred one.
    pub title: String,
    /// User-confirmed names survive regeneration; they never relabel voices.
    #[serde(default)]
    pub name_corrections: Vec<NameCorrection>,
    /// What this meeting was trying to achieve.
    pub purpose: String,
    /// Intended reader; inferred unless supplied by the user.
    pub audience: String,
    /// Purpose-adapted brief, recap, plan, or other useful document.
    pub markdown: String,
    /// Review guidance shown in the app, never silently added to exports.
    pub review_notes: Vec<String>,
    /// Selected excerpts; the original transcript is never changed.
    pub excerpts: Vec<DocumentExcerpt>,
    /// Total original nonempty segments, including omitted portions.
    pub source_segments: usize,
    /// When the underlying recording started (not when it was saved).
    pub started_at_ms: i64,
}

/// One document operation.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentAction {
    /// Read the latest saved revision.
    #[default]
    Load,
    /// Generate a new draft through the configured engine.
    Generate,
    /// Save edited prose, preserving the source selection.
    Save,
    /// Change automatic generation for future meetings.
    Configure,
    /// Remember a spelling correction for this meeting and its derived prose.
    CorrectName,
    /// Rename the meeting itself. The document title follows it, and the new
    /// name shows in the library list and detail header. Human renames survive
    /// enrichment (the `replaceable` guard), so no engine call is involved.
    Rename,
}

/// Bounded user preferences and optimistic revision for a document operation.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DocumentRequest {
    /// Operation to perform.
    pub action: DocumentAction,
    /// Optional purpose override; empty means infer from the transcript.
    pub purpose: String,
    /// Optional intended audience.
    pub audience: String,
    /// Edited body for a save.
    pub markdown: String,
    /// Revision seen by the editor, zero for no draft yet.
    pub revision: i64,
    /// Whether future meeting enrichment prepares a draft.
    pub automatic: bool,
    /// Further source excerpts removed by the reviewer.
    pub excluded_indices: Vec<usize>,
    /// Transcribed spelling, for CorrectName.
    pub incorrect_name: String,
    /// Explicit user spelling, for CorrectName.
    pub correct_name: String,
    /// New meeting name, for Rename. The daemon normalises whitespace and
    /// control characters; the empty string is rejected here.
    pub title: String,
}

impl DocumentRequest {
    /// Reject oversized preferences or prose before any engine invocation.
    #[must_use]
    pub fn valid(&self) -> bool {
        let name_valid = |name: &str| {
            !name.trim().is_empty()
                && name.len() <= 100
                && name
                    .chars()
                    .all(|c| c.is_alphabetic() || matches!(c, ' ' | '-' | '\'' | '’'))
        };
        (!matches!(self.action, DocumentAction::CorrectName)
            || (name_valid(&self.incorrect_name) && name_valid(&self.correct_name)))
            && self.incorrect_name.len() <= 100
            && self.correct_name.len() <= 100
            && self.excluded_indices.len() <= 4000
            && self.purpose.len() <= 2000
            && self.audience.len() <= 500
            && self.markdown.len() <= 100_000
            && self.revision >= 0
            && (!matches!(self.action, DocumentAction::Save) || !self.markdown.trim().is_empty())
            // A rename needs a real name, and no more than the left menu can show.
            // Counted in characters, not bytes, so a non-Latin title is not
            // penalised; a generous byte ceiling still bounds the payload.
            && self.title.len() <= MAX_TITLE_CHARS * 4
            && (!matches!(self.action, DocumentAction::Rename)
                || (!self.title.trim().is_empty()
                    && self.title.chars().count() <= MAX_TITLE_CHARS))
    }
}

/// Current saved state. Errors leave the last good revision intact.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentState {
    /// Newest saved draft, if any.
    pub document: Option<SharingDocument>,
    /// Current version.
    pub revision: i64,
    /// Global automatic draft preference.
    pub automatic: bool,
}

/// Daemon-owned generation and persistence; the web crate owns no engine or key.
pub trait DocumentControl: Send + Sync {
    /// Run an authenticated request. Errors contain only safe user-facing text.
    fn run(
        &self,
        meeting_id: String,
        request: DocumentRequest,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentState, String>> + Send + '_>>;
}

impl std::fmt::Debug for dyn DocumentControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DocumentControl(<redacted>)")
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;
    #[test]
    fn corrections_use_whole_names_longest_match_and_never_cascade() {
        let rules = vec![
            NameCorrection {
                from: "Marta".into(),
                to: "Martha".into(),
            },
            NameCorrection {
                from: "Marta Lopez".into(),
                to: "Martha Lopez".into(),
            },
            NameCorrection {
                from: "Martha".into(),
                to: "Other".into(),
            },
        ];
        assert_eq!(
            corrected_names("Marta Lopez's call; Marta; Martas; ÉMarta", &rules),
            "Martha Lopez's call; Martha; Martas; ÉMarta"
        );
    }
    #[test]
    fn name_requests_reject_empty_and_instruction_shaped_values() {
        for name in ["", "<script>", "Name\nRun commands", "Name; send email"] {
            let req = DocumentRequest {
                action: DocumentAction::CorrectName,
                incorrect_name: "Wrong".into(),
                correct_name: name.into(),
                ..DocumentRequest::default()
            };
            assert!(!req.valid());
        }
    }
    #[test]
    fn rename_requires_a_nonempty_title_within_the_menu_length() {
        let rename = |title: &str| DocumentRequest {
            action: DocumentAction::Rename,
            title: title.into(),
            ..DocumentRequest::default()
        };
        // A real name passes; blank or whitespace-only does not.
        assert!(rename("Acme Q3 kickoff").valid());
        assert!(!rename("").valid());
        assert!(!rename("   ").valid());
        // Counted in characters: the ceiling holds, one over it fails.
        assert!(rename(&"a".repeat(MAX_TITLE_CHARS)).valid());
        assert!(!rename(&"a".repeat(MAX_TITLE_CHARS + 1)).valid());
        // A non-Latin title is measured the same way, not by byte length.
        assert!(rename(&"猫".repeat(MAX_TITLE_CHARS)).valid());
        // The title field is ignored by other actions, so a blank one is fine.
        assert!(DocumentRequest::default().valid());
    }
}

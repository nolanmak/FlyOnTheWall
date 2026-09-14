//! Purpose-aware, locally saved sharing drafts. No operation sends a document.
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

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
    /// Document title, inferred from the meeting's purpose.
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
}

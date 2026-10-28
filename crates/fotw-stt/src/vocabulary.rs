//! Versioned, credential-free vocabulary contract. No filesystem or provider IO.
//! Adapted from nolanmak/Murmur at 9c6723f07e6996dd0aeca87919211cc5519cc7cc (Apache-2.0).
use crate::CustomVocabulary;
use serde::{Deserialize, Serialize};
/// Maximum serialized profile size, including whitespace.
pub const MAX_PROFILE_BYTES: usize = 256 * 1024;
/// A deliberate whole-utterance cue, never an ASR hint or inline replacement.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    /// Name following “insert snippet”.
    pub name: String,
    /// Literal local expansion, including multiline Unicode text.
    pub text: String,
}
/// A local correction target: a word or phrase that local-engine text is
/// corrected to after recognition. Ported from Muesli's `CustomWord`, with the
/// same `matching_threshold` wire name. Never sent to a provider.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Correction {
    /// What the recognizer is expected to produce, matched case-, accent- and
    /// near-miss-insensitively. One to eight words.
    pub word: String,
    /// What to write instead; defaults to `word` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    /// Jaro–Winkler similarity a near miss needs, in
    /// [`MIN_MATCHING_THRESHOLD`, `MAX_MATCHING_THRESHOLD`]; defaults to
    /// [`DEFAULT_MATCHING_THRESHOLD`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matching_threshold: Option<f64>,
}
// JSON cannot encode NaN and validation rejects non-finite thresholds.
impl Eq for Correction {}
/// Muesli's default near-miss threshold.
pub const DEFAULT_MATCHING_THRESHOLD: f64 = 0.85;
/// Lowest accepted per-entry threshold (Muesli's clamp).
pub const MIN_MATCHING_THRESHOLD: f64 = 0.70;
/// Highest accepted per-entry threshold (Muesli's clamp).
pub const MAX_MATCHING_THRESHOLD: f64 = 0.95;
/// Maximum number of explicit local corrections.
pub const MAX_CORRECTIONS: usize = 512;
/// Stable version-1 wire shape, shared by applications and export/import.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Only version 1 is currently supported.
    pub version: u32,
    /// Explicit language tag; legacy files default to English.
    #[serde(default = "default_language")]
    pub language: String,
    /// Provider recognition hints, never deterministic replacements.
    #[serde(default)]
    pub recognition_hints: Vec<String>,
    /// Exact deliberate cues, never sent as provider prompts.
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Additive v1 field: local post-recognition corrections. Recognition
    /// hints are correction targets too; an entry here adds a replacement or
    /// threshold. Omitted when empty so older readers still accept exports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<Correction>,
}
fn default_language() -> String {
    "en".into()
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            version: 1,
            language: default_language(),
            recognition_hints: vec![],
            snippets: vec![],
            corrections: vec![],
        }
    }
}
/// Provider capability assessment; it does not claim improved recognition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HintSelection<'a> {
    /// The profile asks for no recognition hints.
    NotRequested,
    /// The adapter must encode terms using exactly this mechanism.
    Supported {
        /// A keyterm, keyword or prompt mechanism, not interchangeable semantics.
        mechanism: CustomVocabulary,
        /// Only recognition hints, never snippets.
        terms: &'a [String],
    },
    /// The provider cannot apply requested hints; surface this limitation.
    Unsupported,
}
impl Profile {
    /// Validate version, language, hints, snippets and serialized byte budget.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("Unsupported vocabulary schema version".into());
        }
        validate_language(&self.language)?;
        validate_hints(&self.recognition_hints)?;
        if self.snippets.len() > 128 {
            return Err("At most 128 snippets are supported".into());
        }
        let mut names = std::collections::HashSet::new();
        for snippet in &self.snippets {
            let name = &snippet.name;
            if name.is_empty()
                || name.len() > 80
                || name.trim() != name
                || name.split(' ').any(str::is_empty)
                || !name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-'))
            {
                return Err("Snippet names must be 1–80 UTF-8 bytes of letters, numbers, spaces, hyphens or underscores".into());
            }
            if !names.insert(name.to_lowercase()) {
                return Err("Snippet names conflict ignoring case".into());
            }
            if snippet.text.is_empty()
                || snippet.text.len() > 16 * 1024
                || snippet
                    .text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
            {
                return Err("Snippet text must be nonempty, at most 16 KiB, with no controls except newline and tab".into());
            }
        }
        validate_corrections(&self.corrections)?;
        if serde_json::to_vec_pretty(self)
            .map_err(|_| "Cannot encode vocabulary profile")?
            .len()
            > MAX_PROFILE_BYTES
        {
            return Err("Vocabulary profile exceeds 256 KiB".into());
        }
        Ok(())
    }
    /// Parse a bounded, validated profile without exposing its contents on error.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err("Vocabulary profile exceeds 256 KiB".into());
        }
        let profile: Self =
            serde_json::from_slice(bytes).map_err(|_| "Invalid vocabulary profile schema")?;
        profile.validate()?;
        Ok(profile)
    }
    /// Expand only exact deliberate cues; ordinary prose and code stay literal.
    #[must_use]
    pub fn expand(&self, text: &str) -> String {
        self.snippet(text).unwrap_or(text).to_owned()
    }
    /// Resolve only the exact deliberate cue; expansion stays entirely local.
    pub fn snippet(&self, text: &str) -> Option<&str> {
        let cue = text.trim();
        let cue = cue
            .strip_suffix('.')
            .or_else(|| cue.strip_suffix('!'))
            .or_else(|| cue.strip_suffix('?'))
            .unwrap_or(cue)
            .to_lowercase();
        let name = cue.strip_prefix("insert snippet ")?;
        self.snippets
            .iter()
            .find(|s| s.name.to_lowercase() == name)
            .map(|s| s.text.as_str())
    }
    /// Assess capabilities only; each provider owns actual request encoding.
    #[must_use]
    pub fn hints_for(&self, mechanism: CustomVocabulary) -> HintSelection<'_> {
        if self.recognition_hints.is_empty() {
            HintSelection::NotRequested
        } else if mechanism == CustomVocabulary::None {
            HintSelection::Unsupported
        } else {
            HintSelection::Supported {
                mechanism,
                terms: &self.recognition_hints,
            }
        }
    }
}
/// Validate explicit bounded language syntax, not a provider's language support.
pub fn validate_language(tag: &str) -> Result<(), String> {
    let mut parts = tag.split('-');
    let primary = parts.next().unwrap_or_default();
    if tag.len() > 35
        || !(2..=3).contains(&primary.len())
        || !primary.bytes().all(|b| b.is_ascii_lowercase())
        || parts.any(|part| {
            !(2..=8).contains(&part.len()) || !part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    {
        return Err("Choose an explicit language code such as en, es or fr-CA".into());
    }
    Ok(())
}

/// Conservative application budget: UTF-8 bytes, not a provider tokenizer count.
pub fn validate_hints(hints: &[String]) -> Result<(), String> {
    if hints.len() > 64 || hints.iter().map(|h| h.len() + 1).sum::<usize>() > 500 {
        return Err(
            "Recognition hints exceed 64 terms or 500 UTF-8 bytes including separators".into(),
        );
    }
    let mut unique = std::collections::HashSet::new();
    for hint in hints {
        if hint.is_empty()
            || hint.trim() != hint
            || hint.len() > 128
            || hint.chars().any(char::is_control)
        {
            return Err("Recognition hints must be trimmed, nonempty, at most 128 UTF-8 bytes and contain no controls".into());
        }
        if !unique.insert(hint.to_lowercase()) {
            return Err("Recognition hints conflict ignoring case".into());
        }
    }
    Ok(())
}

/// Validate local corrections: trimmed single-spaced words of one to eight
/// words, distinct ignoring case, with bounded replacements and thresholds.
pub fn validate_corrections(corrections: &[Correction]) -> Result<(), String> {
    if corrections.len() > MAX_CORRECTIONS {
        return Err(format!(
            "At most {MAX_CORRECTIONS} vocabulary corrections are supported"
        ));
    }
    let bounded = |text: &str| {
        !text.is_empty()
            && text.len() <= 128
            && text.trim() == text
            && !text.chars().any(char::is_control)
    };
    let mut words = std::collections::HashSet::new();
    for correction in corrections {
        let word = &correction.word;
        let parts: Vec<&str> = word.split(' ').collect();
        if !bounded(word)
            || parts.len() > 8
            || parts
                .iter()
                .any(|part| !crate::vocabulary_matcher::is_matchable_word(part))
        {
            return Err("Correction words must be 1–8 single-spaced words, trimmed, at most 128 UTF-8 bytes, with no controls".into());
        }
        if correction
            .replacement
            .as_deref()
            .is_some_and(|r| !bounded(r))
        {
            return Err("Correction replacements must be trimmed, nonempty, at most 128 UTF-8 bytes and contain no controls".into());
        }
        if correction.matching_threshold.is_some_and(|t| {
            !t.is_finite() || !(MIN_MATCHING_THRESHOLD..=MAX_MATCHING_THRESHOLD).contains(&t)
        }) {
            return Err(format!(
                "Correction thresholds must be between {MIN_MATCHING_THRESHOLD} and {MAX_MATCHING_THRESHOLD}"
            ));
        }
        if !words.insert(word.to_lowercase()) {
            return Err("Correction words conflict ignoring case".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_vocabulary_fixture_contract() {
        crate::conformance::vocabulary();
    }
}

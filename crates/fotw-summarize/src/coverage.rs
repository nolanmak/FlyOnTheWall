//! Citation coverage for Call A (spec 8.6).
//!
//! `coverage = cited_claims / total_claims` over substantive claims, with a
//! banner below 0.7 and a dashed left border on the uncited paragraphs.
//!
//! **Two rules here are about restraint, and both are easy to "improve" into
//! something worse.**
//!
//! *Do not delete uncited claims.* The tempting move is to strip anything
//! without a citation and report 1.0. What that deletes is the connective
//! tissue — "this is the third week the same blocker has come up", an
//! observation no single segment supports and no participant said out loud.
//! That is often the most valuable sentence in the summary. Spec 8.6 says
//! render it with a dashed border and let the reader judge. So
//! [`CoverageReport`] returns the blocks **unmodified** and reports which
//! claims are uncited alongside them.
//!
//! *Call it grounding, never accuracy.* A model can cite a real segment while
//! mischaracterizing what it says; the citation is then honest about its source
//! and wrong about its content, and coverage still reads 1.0. [`METRIC_LABEL`]
//! exists so that the string lives in one place and cannot drift into
//! "accuracy" in a UI three layers up.

use crate::adapter::AnnotatedBlock;

/// Below this, the banner shows (spec 8.6).
pub const LOW_GROUNDING_THRESHOLD: f64 = 0.7;

/// The metric's name. **Never "accuracy"** — see the module docs.
pub const METRIC_LABEL: &str = "transcript grounding";

/// The banner text spec 8.6 specifies.
pub const LOW_GROUNDING_BANNER: &str =
    "This summary has low transcript grounding — review before sharing.";

/// A claim is substantive above this many words (spec 8.6).
pub const MIN_CLAIM_WORDS: usize = 12;

/// What counts as a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageConfig {
    /// Words a sentence needs to be substantive. Spec 8.6 says "> 12".
    pub min_words: usize,
    /// Whether the text of a list item can be a claim.
    ///
    /// Spec 8.6 excludes "a bullet marker", which is ambiguous between *the
    /// marker* and *the whole bullet*. Defaulting to `true` — strip the marker,
    /// judge the content — because meeting notes put most of their substance in
    /// bullets, and excluding them entirely would make coverage a measurement
    /// of the prose around the notes rather than of the notes. Set to `false`
    /// for the literal reading.
    pub include_list_items: bool,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            min_words: MIN_CLAIM_WORDS,
            include_list_items: true,
        }
    }
}

/// One substantive claim found in the generated document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// Index of the response block the claim appeared in.
    pub block_index: usize,
    /// The sentence.
    pub text: String,
    /// Its word count, after markdown markers are stripped.
    pub word_count: usize,
    /// Whether the block carrying it had at least one citation.
    pub cited: bool,
}

/// The result of measuring one generated document.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageReport {
    /// Every substantive claim, cited and uncited alike.
    pub claims: Vec<Claim>,
    /// Substantive claims with at least one citation.
    pub cited_claims: usize,
    /// Substantive claims in total.
    pub total_claims: usize,
}

impl CoverageReport {
    /// `cited / total`, or 1.0 for a document with no substantive claims.
    ///
    /// An empty document is vacuously grounded. Returning 0.0 would banner
    /// every short meeting as ungrounded, training the user to ignore the
    /// banner — which costs more than the edge case is worth.
    #[must_use]
    pub fn coverage(&self) -> f64 {
        if self.total_claims == 0 {
            return 1.0;
        }
        self.cited_claims as f64 / self.total_claims as f64
    }

    /// Whether the banner shows (spec 8.6: below 0.7).
    #[must_use]
    pub fn is_low_grounding(&self) -> bool {
        self.coverage() < LOW_GROUNDING_THRESHOLD
    }

    /// The banner, or `None` when grounding is adequate.
    #[must_use]
    pub fn banner(&self) -> Option<&'static str> {
        self.is_low_grounding().then_some(LOW_GROUNDING_BANNER)
    }

    /// Claims to render with a dashed left border (spec 8.6).
    #[must_use]
    pub fn uncited_claims(&self) -> Vec<&Claim> {
        self.claims.iter().filter(|claim| !claim.cited).collect()
    }
}

/// Measure citation coverage over a generated document.
///
/// Citations attach to response *blocks*: with the Citations API on, the model
/// emits a separate text block for each cited span, so block granularity is
/// close to claim granularity in practice. Every substantive sentence in a
/// block inherits that block's cited/uncited status.
#[must_use]
pub fn measure(blocks: &[AnnotatedBlock], config: &CoverageConfig) -> CoverageReport {
    let mut claims = Vec::new();

    for (block_index, block) in blocks.iter().enumerate() {
        let cited = !block.citations.is_empty();
        for line in block.text.lines() {
            let Some(content) = claim_text(line, config) else {
                continue;
            };
            for sentence in split_sentences(content) {
                let word_count = sentence.split_whitespace().count();
                if word_count > config.min_words {
                    claims.push(Claim {
                        block_index,
                        text: sentence.to_string(),
                        word_count,
                        cited,
                    });
                }
            }
        }
    }

    let cited_claims = claims.iter().filter(|claim| claim.cited).count();
    let total_claims = claims.len();
    CoverageReport {
        claims,
        cited_claims,
        total_claims,
    }
}

/// The claim-bearing text of a line, or `None` if the line is structure.
fn claim_text<'a>(line: &'a str, config: &CoverageConfig) -> Option<&'a str> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Headings are structure: "Decisions and open questions from the weekly
    // planning sync" is twelve words and no kind of claim.
    if trimmed.starts_with('#') {
        return None;
    }
    // A horizontal rule or a table separator is not prose either.
    if trimmed
        .chars()
        .all(|c| matches!(c, '-' | '=' | '*' | '_' | '|' | ' ' | ':'))
    {
        return None;
    }

    if let Some(rest) = strip_list_marker(trimmed) {
        return config.include_list_items.then_some(rest);
    }
    Some(trimmed)
}

/// Strip `-`, `*`, `+` or `1.` from the front of a list item.
fn strip_list_marker(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim_start());
        }
    }
    // Ordered list: digits, then `.` or `)`, then a space.
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let rest = &line[digits.len()..];
        for marker in [". ", ") "] {
            if let Some(rest) = rest.strip_prefix(marker) {
                return Some(rest.trim_start());
            }
        }
    }
    None
}

/// Split a line into sentences on `.`, `!` and `?`.
///
/// Naive on purpose. A real sentence splitter would need an abbreviation list,
/// and the cost of a wrong split here is one claim counted as two — which moves
/// a ratio slightly, not a decision.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let end = index + ch.len_utf8();
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Citation;

    fn citation(block: usize) -> Citation {
        Citation {
            start_block_index: block,
            end_block_index: block + 1,
            cited_text: "something that was said".to_string(),
            document_index: 0,
        }
    }

    fn cited(text: &str) -> AnnotatedBlock {
        AnnotatedBlock {
            text: text.to_string(),
            citations: vec![citation(0)],
        }
    }

    fn uncited(text: &str) -> AnnotatedBlock {
        AnnotatedBlock {
            text: text.to_string(),
            citations: Vec::new(),
        }
    }

    const LONG: &str =
        "The team agreed to move the storage layer to SQLite before the beta ships next month.";
    const SHORT: &str = "We shipped it.";

    #[test]
    fn coverage_is_cited_over_total_substantive_claims() {
        let report = measure(
            &[cited(LONG), cited(LONG), uncited(LONG), uncited(LONG)],
            &CoverageConfig::default(),
        );
        assert_eq!(report.total_claims, 4);
        assert_eq!(report.cited_claims, 2);
        assert!((report.coverage() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_short_sentence_is_not_a_substantive_claim() {
        // Spec 8.6: "> 12 words". A twelve-word sentence is not one; thirteen
        // is. Getting the boundary wrong shifts every ratio in the product.
        let twelve = "one two three four five six seven eight nine ten eleven twelve";
        let thirteen = format!("{twelve} thirteen");
        assert_eq!(twelve.split_whitespace().count(), 12);

        let report = measure(
            &[uncited(twelve), uncited(&thirteen), uncited(SHORT)],
            &CoverageConfig::default(),
        );
        assert_eq!(report.total_claims, 1);
        assert_eq!(report.claims[0].word_count, 13);
    }

    #[test]
    fn headings_are_never_claims_however_long() {
        let heading = "## Decisions and open questions from the weekly planning sync with the team";
        assert!(heading.split_whitespace().count() > MIN_CLAIM_WORDS);
        let report = measure(&[uncited(heading)], &CoverageConfig::default());
        assert_eq!(report.total_claims, 0);
    }

    #[test]
    fn horizontal_rules_and_table_separators_are_not_claims() {
        let report = measure(
            &[uncited("---"), uncited("| --- | --- |"), uncited("***")],
            &CoverageConfig::default(),
        );
        assert_eq!(report.total_claims, 0);
    }

    #[test]
    fn a_bullets_content_is_judged_without_its_marker() {
        // The default reading: the marker is structure, the sentence after it
        // is the claim. Meeting notes live in bullets.
        for line in [
            format!("- {LONG}"),
            format!("* {LONG}"),
            format!("+ {LONG}"),
            format!("1. {LONG}"),
            format!("2) {LONG}"),
        ] {
            let report = measure(&[uncited(&line)], &CoverageConfig::default());
            assert_eq!(report.total_claims, 1, "{line:?} was not counted");
            assert!(!report.claims[0].text.starts_with(['-', '*', '+']));
        }
    }

    #[test]
    fn the_literal_reading_of_spec_8_6_is_available_as_config() {
        let config = CoverageConfig {
            include_list_items: false,
            ..CoverageConfig::default()
        };
        let report = measure(&[uncited(&format!("- {LONG}"))], &config);
        assert_eq!(report.total_claims, 0);
    }

    #[test]
    fn several_sentences_in_one_block_are_separate_claims() {
        let block = format!("{LONG} {LONG}");
        let report = measure(&[cited(&block)], &CoverageConfig::default());
        assert_eq!(report.total_claims, 2);
        assert_eq!(report.cited_claims, 2);
    }

    #[test]
    fn low_grounding_fires_below_zero_point_seven_and_not_at_it() {
        // 7 of 10 is exactly the threshold and must NOT banner; 6 of 10 must.
        let mut blocks: Vec<AnnotatedBlock> = (0..7).map(|_| cited(LONG)).collect();
        blocks.extend((0..3).map(|_| uncited(LONG)));
        let at_threshold = measure(&blocks, &CoverageConfig::default());
        assert!((at_threshold.coverage() - 0.7).abs() < 1e-9);
        assert!(!at_threshold.is_low_grounding());
        assert!(at_threshold.banner().is_none());

        let mut blocks: Vec<AnnotatedBlock> = (0..6).map(|_| cited(LONG)).collect();
        blocks.extend((0..4).map(|_| uncited(LONG)));
        let below = measure(&blocks, &CoverageConfig::default());
        assert!(below.is_low_grounding());
        assert_eq!(below.banner(), Some(LOW_GROUNDING_BANNER));
    }

    #[test]
    fn uncited_claims_are_reported_never_deleted() {
        // Spec 8.6 is explicit: do not silently delete them. Deletion loses
        // genuinely-inferred connective tissue, which is often the only
        // sentence in the summary the reader could not have written themselves.
        let inference =
            "This is the third meeting in a row where the same migration blocker has come up.";
        let blocks = vec![cited(LONG), uncited(inference)];
        let report = measure(&blocks, &CoverageConfig::default());

        assert_eq!(report.total_claims, 2);
        let uncited_claims = report.uncited_claims();
        assert_eq!(uncited_claims.len(), 1);
        assert_eq!(uncited_claims[0].text, inference);
        // And the caller's blocks are untouched -- `measure` takes a slice and
        // returns a report, so there is no path by which it could edit them.
        assert_eq!(blocks[1].text, inference);
    }

    #[test]
    fn a_claim_points_back_at_the_block_it_came_from() {
        // The UI needs this to draw the dashed border in the right place.
        let report = measure(
            &[cited(LONG), uncited(LONG), cited(LONG)],
            &CoverageConfig::default(),
        );
        assert_eq!(report.uncited_claims()[0].block_index, 1);
    }

    #[test]
    fn a_document_with_no_substantive_claims_is_vacuously_grounded() {
        let report = measure(
            &[uncited(SHORT), uncited("# Notes")],
            &CoverageConfig::default(),
        );
        assert_eq!(report.total_claims, 0);
        assert!((report.coverage() - 1.0).abs() < f64::EPSILON);
        assert!(!report.is_low_grounding());
    }

    #[test]
    fn the_metric_is_called_grounding_and_not_accuracy() {
        // Spec 8.6 is emphatic: a model can cite a real segment while
        // mischaracterizing it, so coverage of 1.0 does not mean zero
        // hallucination. The name is the only thing stopping a reader from
        // concluding otherwise, and it lives here so a UI cannot rename it.
        assert_eq!(METRIC_LABEL, "transcript grounding");
        assert!(!METRIC_LABEL.contains("accur"));
        assert!(LOW_GROUNDING_BANNER.contains("transcript grounding"));
        assert!(!LOW_GROUNDING_BANNER.contains("accur"));
    }
}

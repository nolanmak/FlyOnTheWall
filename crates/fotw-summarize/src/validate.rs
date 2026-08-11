//! The evidence validator (spec 8.6).
//!
//! Spec 8.6 calls this "the highest-leverage mechanism in the system", and the
//! reason is that it is the only anti-hallucination control here that does not
//! ask the model for anything. The prompt contract is a request. Citations are
//! a server-side guarantee from one vendor. This is arithmetic on strings, it
//! runs identically against Opus 5 and a quantized 7B behind Ollama, and it
//! cannot be talked out of its answer by anything a participant says on the
//! call.
//!
//! Four rules, and the asymmetry between them is deliberate:
//!
//! | # | Check | Failure |
//! |---|---|---|
//! | 1 | every `evidence_segment_ids` entry exists | **drop** the item |
//! | 2 | `evidence_quote` is a substring of the cited segments | **drop** the item |
//! | 3 | non-null `owner` matches a speaker or a proper noun in a cited segment | null `owner`, mark `implied` |
//! | 4 | non-null `due` has its `due_raw` in a cited segment | null `due`, mark `implied` |
//!
//! Rules 1 and 2 mean the item is not attached to reality at all — there is
//! nothing to show and nothing to fix, so it goes. Rules 3 and 4 mean the item
//! is real but one field on it is not, and deleting a genuine commitment
//! because its date was wrong would lose more than it saved.
//!
//! **The gap this does not close, stated in spec 8.6 and worth repeating where
//! someone might otherwise trust the output:** a diarization error that swaps
//! speakers produces a confidently-cited action item assigned to the wrong
//! person, and every rule above passes it, because the segment genuinely says
//! what the model quoted. The partial mitigation is
//! [`ValidatorConfig::min_speaker_confidence`] — refuse to auto-assign an owner
//! out of a segment the STT provider was unsure about.

use std::collections::HashSet;

use crate::document::TranscriptDocument;
use crate::schema::{Confidence, Extraction, ItemKind};

/// Below this segment confidence, an owner is never auto-assigned.
///
/// Spec 8.6's stated mitigation for the diarization gap. 0.6 is a judgement
/// call, not a measured threshold: Deepgram's per-word confidences cluster well
/// above it on clean audio, so it fires on the crosstalk and overlap where
/// speaker attribution is actually unreliable.
pub const DEFAULT_MIN_SPEAKER_CONFIDENCE: f64 = 0.6;

/// Knobs on the validator.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatorConfig {
    /// See [`DEFAULT_MIN_SPEAKER_CONFIDENCE`]. Set to 0.0 to disable the gate.
    pub min_speaker_confidence: f64,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            min_speaker_confidence: DEFAULT_MIN_SPEAKER_CONFIDENCE,
        }
    }
}

/// Why an item was dropped (spec 8.6 rules 1 and 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// `evidence_segment_ids` was empty. The schema says `minItems: 1`, so
    /// this only happens on a provider without strict-mode enforcement.
    NoEvidence,
    /// The model named a segment id that does not exist.
    UnknownSegmentId(usize),
    /// The quote is not a substring of the cited segments.
    QuoteNotInCitedSegments,
}

/// Why a field was nulled (spec 8.6 rules 3 and 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adjustment {
    /// The owner is neither a known speaker label nor a proper noun in a cited
    /// segment.
    OwnerUnverifiable,
    /// The cited segments' speaker attribution was too uncertain to assign an
    /// owner from.
    OwnerLowConfidence,
    /// `due_raw` does not appear in a cited segment, so the date is
    /// unverifiable.
    DueUnverifiable,
}

/// An item that did not survive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedItem {
    /// Which list it came from.
    pub kind: ItemKind,
    /// Its text, for the "show anyway?" affordance.
    pub text: String,
    /// Why.
    pub reason: DropReason,
}

/// A field that was nulled on an item that survived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjustedItem {
    /// Which list it came from.
    pub kind: ItemKind,
    /// The item's text.
    pub text: String,
    /// What was nulled and why.
    pub adjustment: Adjustment,
}

/// What the validator produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    /// The extraction with unverifiable items removed and unverifiable fields
    /// nulled.
    pub extraction: Extraction,
    /// Items removed (spec 8.6 rules 1 and 2). Logged, never silently lost.
    pub dropped: Vec<DroppedItem>,
    /// Fields nulled (spec 8.6 rules 3 and 4).
    pub adjusted: Vec<AdjustedItem>,
}

impl ValidationReport {
    /// How many items were dropped.
    #[must_use]
    pub fn drop_count(&self) -> usize {
        self.dropped.len()
    }

    /// The user-facing notice spec 8.6 asks for, or `None` if nothing dropped.
    #[must_use]
    pub fn drop_notice(&self) -> Option<String> {
        let count = self.drop_count();
        if count == 0 {
            return None;
        }
        let plural = if count == 1 { "item" } else { "items" };
        Some(format!(
            "{count} candidate {plural} had no verifiable evidence and were hidden — show anyway?"
        ))
    }
}

/// Run the four rules of spec 8.6 over an extraction.
///
/// Never fails: an unverifiable extraction produces an empty one and a full
/// [`ValidationReport::dropped`] list, because "the model returned nothing
/// checkable" is a result to render, not an error to propagate.
#[must_use]
pub fn validate(
    extraction: &Extraction,
    document: &TranscriptDocument,
    config: &ValidatorConfig,
) -> ValidationReport {
    let mut report = ValidationReport {
        extraction: Extraction::default(),
        dropped: Vec::new(),
        adjusted: Vec::new(),
    };
    let speakers = document.speaker_labels();

    for item in &extraction.action_items {
        let kind = ItemKind::ActionItem;
        let Some(cited) = report.check_evidence(
            kind,
            &item.text,
            &item.evidence_segment_ids,
            &item.evidence_quote,
            document,
        ) else {
            continue;
        };
        let mut item = item.clone();

        // Rules 3 and 4 null a field and downgrade confidence, rather than
        // dropping: the commitment is real even when its date is not.
        if let Some(owner) = item.owner.clone()
            && let Some(adjustment) = unverifiable_owner(&owner, &cited, &speakers, config)
        {
            item.owner = None;
            item.confidence = Confidence::Implied;
            report.record_adjustment(kind, &item.text, adjustment);
        }
        if item.due.is_some() && !due_is_verifiable(item.due_raw.as_deref(), &cited) {
            item.due = None;
            item.due_raw = None;
            item.confidence = Confidence::Implied;
            report.record_adjustment(kind, &item.text, Adjustment::DueUnverifiable);
        }
        report.extraction.action_items.push(item);
    }

    for item in &extraction.decisions {
        if report
            .check_evidence(
                ItemKind::Decision,
                &item.text,
                &item.evidence_segment_ids,
                &item.evidence_quote,
                document,
            )
            .is_some()
        {
            report.extraction.decisions.push(item.clone());
        }
    }

    for item in &extraction.open_questions {
        let kind = ItemKind::OpenQuestion;
        let Some(cited) = report.check_evidence(
            kind,
            &item.text,
            &item.evidence_segment_ids,
            &item.evidence_quote,
            document,
        ) else {
            continue;
        };
        let mut item = item.clone();
        // `raised_by` is an owner-shaped field and gets the owner-shaped check:
        // attributing a question to somebody who was not in the cited segment
        // is the same failure as attributing a commitment to them.
        if let Some(raised_by) = item.raised_by.clone()
            && let Some(adjustment) = unverifiable_owner(&raised_by, &cited, &speakers, config)
        {
            item.raised_by = None;
            item.confidence = Confidence::Implied;
            report.record_adjustment(kind, &item.text, adjustment);
        }
        report.extraction.open_questions.push(item);
    }

    for item in &extraction.follow_ups {
        if report
            .check_evidence(
                ItemKind::FollowUp,
                &item.text,
                &item.evidence_segment_ids,
                &item.evidence_quote,
                document,
            )
            .is_some()
        {
            report.extraction.follow_ups.push(item.clone());
        }
    }

    for topic in &extraction.topics {
        // A topic carries no quote, only a start id -- but an id that does not
        // resolve cannot be placed on the scrubber, so rule 1 still applies.
        if document.segment(topic.start_segment_id).is_some() {
            report.extraction.topics.push(topic.clone());
        } else {
            report.dropped.push(DroppedItem {
                kind: ItemKind::Topic,
                text: topic.label.clone(),
                reason: DropReason::UnknownSegmentId(topic.start_segment_id),
            });
        }
    }

    report
}

impl ValidationReport {
    /// Rules 1 and 2. Returns the cited segments on success, `None` on a drop.
    fn check_evidence(
        &mut self,
        kind: ItemKind,
        text: &str,
        ids: &[usize],
        quote: &str,
        document: &TranscriptDocument,
    ) -> Option<CitedSegments> {
        let mut drop = |reason| {
            self.dropped.push(DroppedItem {
                kind,
                text: text.to_string(),
                reason,
            });
            None::<CitedSegments>
        };

        if ids.is_empty() {
            return drop(DropReason::NoEvidence);
        }
        for &id in ids {
            if document.segment(id).is_none() {
                return drop(DropReason::UnknownSegmentId(id));
            }
        }
        let Some(concatenated) = document.concatenated_text(ids) else {
            // Unreachable given the loop above; a drop is the safe direction.
            return drop(DropReason::NoEvidence);
        };

        let normalized = normalize(&concatenated);
        if !normalized.contains(&normalize(quote)) {
            return drop(DropReason::QuoteNotInCitedSegments);
        }

        let min_confidence = ids
            .iter()
            .filter_map(|&id| document.segment(id).and_then(|segment| segment.confidence))
            .fold(f64::INFINITY, f64::min);

        Some(CitedSegments {
            text: concatenated,
            normalized,
            min_confidence,
        })
    }

    fn record_adjustment(&mut self, kind: ItemKind, text: &str, adjustment: Adjustment) {
        self.adjusted.push(AdjustedItem {
            kind,
            text: text.to_string(),
            adjustment,
        });
    }
}

/// The segments an item cited, prepared once for the field-level rules.
struct CitedSegments {
    text: String,
    normalized: String,
    /// Lowest reported confidence among the cited segments, or `f64::INFINITY`
    /// when none of them reported one. Infinity rather than 0.0 on purpose:
    /// "no confidence reported" is an OpenAI-streaming fact of life (spec 7.2)
    /// and must not read as "the provider was unsure".
    min_confidence: f64,
}

/// Spec 8.6 rule 3, plus the low-confidence mitigation.
///
/// Returns the reason the owner cannot stand, or `None` if it can.
fn unverifiable_owner(
    owner: &str,
    cited: &CitedSegments,
    speakers: &[String],
    config: &ValidatorConfig,
) -> Option<Adjustment> {
    if cited.min_confidence < config.min_speaker_confidence {
        return Some(Adjustment::OwnerLowConfidence);
    }
    let owner_normalized = normalize(owner);
    if owner_normalized.is_empty() {
        return Some(Adjustment::OwnerUnverifiable);
    }
    if speakers
        .iter()
        .any(|label| normalize(label) == owner_normalized)
    {
        return None;
    }
    let nouns = proper_nouns(&cited.text);
    let all_words_are_proper_nouns = owner_normalized
        .split_whitespace()
        .all(|word| nouns.contains(word));
    if all_words_are_proper_nouns {
        None
    } else {
        Some(Adjustment::OwnerUnverifiable)
    }
}

/// Spec 8.6 rule 4: the literal phrase has to appear in a cited segment.
fn due_is_verifiable(due_raw: Option<&str>, cited: &CitedSegments) -> bool {
    // A resolved date with no raw phrase is unverifiable by construction: the
    // model did the arithmetic silently and left nothing to check.
    let Some(raw) = due_raw else {
        return false;
    };
    let normalized = normalize(raw);
    !normalized.is_empty() && cited.normalized.contains(&normalized)
}

/// Lowercase and collapse whitespace (spec 8.6 rule 2).
///
/// Deliberately *only* whitespace and case. Stripping punctuation as well
/// would let "we should ship" match "we should. Ship —" and would start
/// eroding the one check in this crate that is not a judgement call.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words in `text` that look like proper nouns, lowercased.
///
/// A heuristic, and one with a deliberate bias. A token counts when it starts
/// with an uppercase letter **and** is not sentence-initial, because in
/// "Somebody needs to update the docs" the capital is grammar, not a name —
/// and accepting sentence-initial capitals would let a model launder almost any
/// invented owner through the first word of a cited sentence.
///
/// The cost is a false negative on a name that only ever appears at the start
/// of a sentence, which nulls an owner that was arguably real. That is the
/// direction spec 8.5 asks to fail in: a null owner is a correct and expected
/// answer, a guessed one is a failure.
fn proper_nouns(text: &str) -> HashSet<String> {
    let mut nouns = HashSet::new();
    let mut at_sentence_start = true;
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric());
        let ends_sentence = token.ends_with(['.', '!', '?']);
        if trimmed.is_empty() {
            at_sentence_start |= ends_sentence;
            continue;
        }
        let starts_upper = trimmed.chars().next().is_some_and(char::is_uppercase);
        if starts_upper && !at_sentence_start {
            nouns.insert(trimmed.to_lowercase());
        }
        at_sentence_start = ends_sentence;
    }
    nouns
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ActionItem, Decision, FollowUp, OpenQuestion, Topic};
    use crate::testing::{sample_meeting, segment};

    fn document() -> TranscriptDocument {
        TranscriptDocument::from_segments(&sample_meeting())
    }

    fn action(
        text: &str,
        owner: Option<&str>,
        due: Option<&str>,
        due_raw: Option<&str>,
        ids: Vec<usize>,
        quote: &str,
    ) -> ActionItem {
        ActionItem {
            text: text.to_string(),
            owner: owner.map(str::to_string),
            due: due.map(str::to_string),
            due_raw: due_raw.map(str::to_string),
            confidence: Confidence::Explicit,
            evidence_segment_ids: ids,
            evidence_quote: quote.to_string(),
        }
    }

    fn only(items: Vec<ActionItem>) -> Extraction {
        Extraction {
            action_items: items,
            ..Extraction::default()
        }
    }

    fn run(extraction: &Extraction) -> ValidationReport {
        validate(extraction, &document(), &ValidatorConfig::default())
    }

    #[test]
    fn a_well_evidenced_item_survives_untouched() {
        let item = action(
            "Write the migration script",
            Some("S0"),
            Some("2026-08-14"),
            Some("by Friday"),
            vec![2],
            "I will write the migration script by Friday",
        );
        let report = run(&only(vec![item.clone()]));
        assert_eq!(report.extraction.action_items, vec![item]);
        assert!(report.dropped.is_empty());
        assert!(report.adjusted.is_empty());
    }

    #[test]
    fn rule_1_drops_an_item_citing_a_segment_that_does_not_exist() {
        let report = run(&only(vec![action(
            "Invented",
            None,
            None,
            None,
            vec![99],
            "I will write the migration script",
        )]));
        assert!(report.extraction.action_items.is_empty());
        assert_eq!(report.dropped.len(), 1);
        assert_eq!(report.dropped[0].reason, DropReason::UnknownSegmentId(99));
    }

    #[test]
    fn rule_1_drops_an_item_with_no_evidence_at_all() {
        let report = run(&only(vec![action(
            "Unsupported",
            None,
            None,
            None,
            vec![],
            "anything",
        )]));
        assert!(report.extraction.action_items.is_empty());
        assert_eq!(report.dropped[0].reason, DropReason::NoEvidence);
    }

    #[test]
    fn rule_1_drops_when_only_one_of_several_ids_is_bad() {
        // Validating against the subset that happened to exist would let a
        // model launder a fabricated id by pairing it with a real one.
        let report = run(&only(vec![action(
            "Half real",
            None,
            None,
            None,
            vec![2, 42],
            "I will write the migration script",
        )]));
        assert!(report.extraction.action_items.is_empty());
        assert_eq!(report.dropped[0].reason, DropReason::UnknownSegmentId(42));
    }

    #[test]
    fn rule_2_drops_an_item_whose_quote_is_not_in_the_cited_segments() {
        let report = run(&only(vec![action(
            "Fabricated quote",
            None,
            None,
            None,
            vec![2],
            "I will rewrite the entire backend this weekend",
        )]));
        assert!(report.extraction.action_items.is_empty());
        assert_eq!(
            report.dropped[0].reason,
            DropReason::QuoteNotInCitedSegments
        );
    }

    #[test]
    fn rule_2_drops_a_quote_that_exists_elsewhere_but_not_in_the_cited_segment() {
        // The subtle case: every word is real, the citation points somewhere
        // else. Without this the citation would be decorative.
        let report = run(&only(vec![action(
            "Wrong citation",
            None,
            None,
            None,
            vec![0],
            "I will write the migration script by Friday",
        )]));
        assert!(report.extraction.action_items.is_empty());
        assert_eq!(
            report.dropped[0].reason,
            DropReason::QuoteNotInCitedSegments
        );
    }

    #[test]
    fn rule_2_normalizes_whitespace_and_case_before_matching() {
        // Spec 8.6: whitespace-normalized and lowercased. A model that
        // re-wraps its quote across lines is not hallucinating.
        let report = run(&only(vec![action(
            "Reformatted quote",
            None,
            None,
            None,
            vec![2],
            "  I WILL   write\n the MIGRATION\tscript  ",
        )]));
        assert_eq!(report.extraction.action_items.len(), 1);
        assert!(report.dropped.is_empty());
    }

    #[test]
    fn rule_2_matches_across_the_concatenation_of_several_cited_segments() {
        let report = run(&only(vec![action(
            "Spanning quote",
            None,
            None,
            None,
            vec![2, 3],
            "by Friday. Somebody needs to update the docs",
        )]));
        assert_eq!(report.extraction.action_items.len(), 1);
    }

    #[test]
    fn rule_3_nulls_an_owner_who_never_appears_in_the_transcript() {
        // The headline anti-invention case: the quote is real, the citation is
        // real, and the owner is a name nobody said.
        let report = run(&only(vec![action(
            "Write the migration script",
            Some("Sarah"),
            None,
            None,
            vec![2],
            "I will write the migration script by Friday",
        )]));
        assert_eq!(report.extraction.action_items.len(), 1, "the item is real");
        let item = &report.extraction.action_items[0];
        assert_eq!(item.owner, None, "an invented owner must be nulled");
        assert_eq!(item.confidence, Confidence::Implied);
        assert_eq!(report.adjusted[0].adjustment, Adjustment::OwnerUnverifiable);
    }

    #[test]
    fn rule_3_accepts_a_known_speaker_label() {
        let report = run(&only(vec![action(
            "Write the migration script",
            Some("S0"),
            None,
            None,
            vec![2],
            "I will write the migration script",
        )]));
        assert_eq!(
            report.extraction.action_items[0].owner.as_deref(),
            Some("S0")
        );
        assert_eq!(
            report.extraction.action_items[0].confidence,
            Confidence::Explicit
        );
    }

    #[test]
    fn rule_3_accepts_a_proper_noun_spoken_in_a_cited_segment() {
        // "Alice here." is in segment 0; an owner of Alice cited to segment 0
        // is grounded even though the diarization label is S0.
        let report = run(&only(vec![action(
            "Kick off the migration",
            Some("Alice"),
            None,
            None,
            vec![0],
            "Let's start with the migration",
        )]));
        assert_eq!(
            report.extraction.action_items[0].owner.as_deref(),
            Some("Alice")
        );
        assert!(report.adjusted.is_empty());
    }

    #[test]
    fn rule_3_rejects_a_proper_noun_from_a_segment_that_was_not_cited() {
        // Alice is in the transcript, but not in the segment this item cites.
        let report = run(&only(vec![action(
            "Update the docs",
            Some("Alice"),
            None,
            None,
            vec![3],
            "Somebody needs to update the docs",
        )]));
        assert_eq!(report.extraction.action_items[0].owner, None);
        assert_eq!(report.adjusted[0].adjustment, Adjustment::OwnerUnverifiable);
    }

    #[test]
    fn rule_3_does_not_promote_an_ordinary_sentence_opening_word_to_a_name() {
        // "Somebody needs to update the docs" starts with a capital. Accepting
        // sentence-initial capitals as proper nouns would make the check pass
        // for owner: "Somebody", "We", "Open" and most of the transcript.
        let report = run(&only(vec![action(
            "Update the docs",
            Some("Somebody"),
            None,
            None,
            vec![3],
            "Somebody needs to update the docs",
        )]));
        assert_eq!(report.extraction.action_items[0].owner, None);
    }

    #[test]
    fn rule_3_refuses_to_assign_an_owner_from_a_low_confidence_segment() {
        // Spec 8.6's mitigation for the diarization gap it says the validator
        // cannot otherwise close.
        let mut segments = sample_meeting();
        segments[2].confidence = Some(0.31);
        let document = TranscriptDocument::from_segments(&segments);

        let extraction = only(vec![action(
            "Write the migration script",
            Some("S0"),
            None,
            None,
            vec![2],
            "I will write the migration script",
        )]);
        let report = validate(&extraction, &document, &ValidatorConfig::default());

        assert_eq!(report.extraction.action_items.len(), 1);
        assert_eq!(report.extraction.action_items[0].owner, None);
        assert_eq!(
            report.extraction.action_items[0].confidence,
            Confidence::Implied
        );
        assert_eq!(
            report.adjusted[0].adjustment,
            Adjustment::OwnerLowConfidence
        );
    }

    #[test]
    fn rule_4_nulls_a_due_date_whose_phrase_was_never_said() {
        let report = run(&only(vec![action(
            "Update the docs",
            None,
            Some("2026-03-14"),
            Some("by March 14th"),
            vec![3],
            "Somebody needs to update the docs",
        )]));
        let item = &report.extraction.action_items[0];
        assert_eq!(item.due, None);
        assert_eq!(item.due_raw, None, "the raw phrase goes with the date");
        assert_eq!(item.confidence, Confidence::Implied);
        assert_eq!(report.adjusted[0].adjustment, Adjustment::DueUnverifiable);
    }

    #[test]
    fn rule_4_accepts_a_due_date_whose_phrase_was_said() {
        let report = run(&only(vec![action(
            "Write the migration script",
            None,
            Some("2026-08-14"),
            Some("by Friday"),
            vec![2],
            "I will write the migration script by Friday",
        )]));
        let item = &report.extraction.action_items[0];
        assert_eq!(item.due.as_deref(), Some("2026-08-14"));
        assert_eq!(item.due_raw.as_deref(), Some("by Friday"));
        assert_eq!(item.confidence, Confidence::Explicit);
    }

    #[test]
    fn rule_4_nulls_a_due_date_with_no_raw_phrase_to_check_against() {
        // A resolved ISO date with no literal phrase is unverifiable by
        // construction -- the model did the arithmetic in its head and there
        // is nothing to compare to the transcript.
        let report = run(&only(vec![action(
            "Write the migration script",
            None,
            Some("2026-08-14"),
            None,
            vec![2],
            "I will write the migration script by Friday",
        )]));
        assert_eq!(report.extraction.action_items[0].due, None);
        assert_eq!(report.adjusted[0].adjustment, Adjustment::DueUnverifiable);
    }

    #[test]
    fn acceptance_an_invented_due_date_survives_on_no_item() {
        // Spec 8.6's acceptance criterion, adversarially: the model invents
        // "2026-03-14" everywhere it can, attaching it to a well-evidenced
        // item, a badly-cited item, an item quoting a real phrase from the
        // wrong segment, and one with a fabricated segment id. Zero surviving
        // items may carry that date.
        const INVENTED_ISO: &str = "2026-03-14";
        const INVENTED_RAW: &str = "by March 14th";

        let extraction = only(vec![
            action(
                "Write the migration script",
                Some("S0"),
                Some(INVENTED_ISO),
                Some(INVENTED_RAW),
                vec![2],
                "I will write the migration script by Friday",
            ),
            action(
                "Update the docs",
                Some("Sarah"),
                Some(INVENTED_ISO),
                Some(INVENTED_RAW),
                vec![3],
                "Somebody needs to update the docs",
            ),
            action(
                "Decide on the export format",
                None,
                Some(INVENTED_ISO),
                Some(INVENTED_RAW),
                vec![4],
                "we all agreed the deadline is March 14th",
            ),
            action(
                "Ship the beta",
                None,
                Some(INVENTED_ISO),
                Some(INVENTED_RAW),
                vec![77],
                "Open question is whether we keep the old export format",
            ),
        ]);

        let report = run(&extraction);

        for item in &report.extraction.action_items {
            assert_ne!(
                item.due.as_deref(),
                Some(INVENTED_ISO),
                "an invented due date survived on {:?}",
                item.text
            );
            assert!(
                item.due_raw
                    .as_deref()
                    .is_none_or(|raw| !raw.contains("March")),
                "an invented due phrase survived on {:?}",
                item.text
            );
        }
        // And the surviving items are exactly the two that were genuinely
        // evidenced -- the validator must not have achieved zero by dropping
        // everything.
        assert_eq!(report.extraction.action_items.len(), 2);
        assert_eq!(report.dropped.len(), 2);
    }

    #[test]
    fn every_item_kind_is_validated_not_just_action_items() {
        // A validator that only walked action_items would pass most of the
        // tests above while leaving three quarters of the output ungrounded.
        let extraction = Extraction {
            action_items: vec![action("bad", None, None, None, vec![99], "x")],
            decisions: vec![Decision {
                text: "bad".to_string(),
                alternatives_considered: Vec::new(),
                confidence: Confidence::Explicit,
                evidence_segment_ids: vec![99],
                evidence_quote: "x".to_string(),
            }],
            open_questions: vec![OpenQuestion {
                text: "bad".to_string(),
                raised_by: None,
                confidence: Confidence::Explicit,
                evidence_segment_ids: vec![99],
                evidence_quote: "x".to_string(),
            }],
            follow_ups: vec![FollowUp {
                text: "bad".to_string(),
                blocked_on: None,
                confidence: Confidence::Explicit,
                evidence_segment_ids: vec![99],
                evidence_quote: "x".to_string(),
            }],
            topics: vec![Topic {
                label: "bad".to_string(),
                start_segment_id: 99,
            }],
        };

        let report = run(&extraction);
        assert!(report.extraction.action_items.is_empty());
        assert!(report.extraction.decisions.is_empty());
        assert!(report.extraction.open_questions.is_empty());
        assert!(report.extraction.follow_ups.is_empty());
        assert!(report.extraction.topics.is_empty());
        assert_eq!(report.dropped.len(), 5);

        let kinds: HashSet<ItemKind> = report.dropped.iter().map(|d| d.kind).collect();
        assert_eq!(kinds.len(), 5, "every kind must be reported distinctly");
    }

    #[test]
    fn a_decisions_alternatives_are_not_evidence_checked_but_the_decision_is() {
        // alternatives_considered is prose about what was rejected; requiring
        // each alternative to be independently quotable would drop most real
        // decisions.
        let extraction = Extraction {
            decisions: vec![Decision {
                text: "Use SQLite".to_string(),
                alternatives_considered: vec!["Postgres".to_string(), "DuckDB".to_string()],
                confidence: Confidence::Explicit,
                evidence_segment_ids: vec![1],
                evidence_quote: "We agreed to move the storage layer to SQLite".to_string(),
            }],
            ..Extraction::default()
        };
        let report = run(&extraction);
        assert_eq!(report.extraction.decisions.len(), 1);
        assert_eq!(
            report.extraction.decisions[0].alternatives_considered.len(),
            2
        );
    }

    #[test]
    fn the_drop_notice_matches_the_wording_spec_8_6_asks_for() {
        let clean = run(&Extraction::default());
        assert!(clean.drop_notice().is_none());

        let report = run(&only(vec![
            action("a", None, None, None, vec![99], "x"),
            action("b", None, None, None, vec![98], "x"),
        ]));
        let notice = report.drop_notice().expect("two items dropped");
        assert!(notice.starts_with("2 candidate items had no verifiable evidence"));
        assert!(notice.contains("show anyway?"));
    }

    #[test]
    fn dropped_items_keep_their_text_so_the_user_can_ask_to_see_them() {
        // "Hidden" and "deleted" are different promises. The UI offers "show
        // anyway", so the payload has to survive the drop.
        let report = run(&only(vec![action(
            "Ship the rewrite on Tuesday",
            None,
            None,
            None,
            vec![99],
            "x",
        )]));
        assert_eq!(report.dropped[0].text, "Ship the rewrite on Tuesday");
        assert_eq!(report.dropped[0].kind, ItemKind::ActionItem);
    }

    #[test]
    fn an_empty_transcript_drops_everything_rather_than_accepting_anything() {
        let empty = TranscriptDocument::from_segments(&[]);
        let extraction = only(vec![action("x", None, None, None, vec![0], "x")]);
        let report = validate(&extraction, &empty, &ValidatorConfig::default());
        assert!(report.extraction.action_items.is_empty());
    }

    #[test]
    fn a_segment_with_no_reported_confidence_does_not_block_owner_assignment() {
        // Confidence is `None` for OpenAI streaming (spec 7.2). Treating
        // "unreported" as "low" would null every owner on that provider.
        let mut segments = sample_meeting();
        segments[2].confidence = None;
        let document = TranscriptDocument::from_segments(&segments);
        let extraction = only(vec![action(
            "Write the migration script",
            Some("S0"),
            None,
            None,
            vec![2],
            "I will write the migration script",
        )]);
        let report = validate(&extraction, &document, &ValidatorConfig::default());
        assert_eq!(
            report.extraction.action_items[0].owner.as_deref(),
            Some("S0")
        );
    }

    #[test]
    fn a_name_only_ever_spoken_sentence_initially_is_a_known_false_negative() {
        // Documenting the heuristic's cost rather than pretending it has none.
        // "Alice here." puts the only occurrence of the name where a capital is
        // grammar, so the owner is nulled even though it was arguably real.
        // That is the direction spec 8.5 asks to fail in: a null owner is a
        // correct answer, a guessed one is a failure. If this ever starts
        // passing, the heuristic was loosened and
        // `rule_3_does_not_promote_an_ordinary_sentence_opening_word_to_a_name`
        // is the test that will tell you what it cost.
        let document = TranscriptDocument::from_segments(&[segment(
            "a",
            "S0",
            0,
            1_000,
            "Alice here. I will send the notes.",
        )]);
        let extraction = only(vec![action(
            "Send the notes",
            Some("Alice"),
            None,
            None,
            vec![0],
            "I will send the notes",
        )]);
        let report = validate(&extraction, &document, &ValidatorConfig::default());
        assert_eq!(report.extraction.action_items[0].owner, None);
        assert_eq!(report.adjusted[0].adjustment, Adjustment::OwnerUnverifiable);
    }

    #[test]
    fn proper_nouns_are_found_mid_sentence_and_stripped_of_punctuation() {
        let document = TranscriptDocument::from_segments(&[segment(
            "a",
            "S0",
            0,
            1_000,
            "I spoke to Priya, and then Marcus. Then we stopped.",
        )]);
        let extraction = only(vec![action(
            "Follow up",
            Some("Marcus"),
            None,
            None,
            vec![0],
            "I spoke to Priya",
        )]);
        let report = validate(&extraction, &document, &ValidatorConfig::default());
        assert_eq!(
            report.extraction.action_items[0].owner.as_deref(),
            Some("Marcus")
        );
    }
}

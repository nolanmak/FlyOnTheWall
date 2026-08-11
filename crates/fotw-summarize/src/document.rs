//! The transcript as an Anthropic custom-content document (spec 8.4).
//!
//! Spec 8.4: pass the transcript as a `document` block with
//! `source.type: "content"` and **one text block per transcript segment**, so
//! that a returned citation's `start_block_index` maps 1:1 to a segment and
//! therefore to a timestamp. Anthropic explicitly recommends custom-content
//! documents for transcripts because no further chunking is applied — which is
//! the property the whole citation-to-audio-seek feature (SUM-05) rests on.
//!
//! The index is the contract. A [`TranscriptDocument`] is an ordered, gap-free
//! `Vec` whose position *is* the evidence id the model returns, and
//! [`TranscriptDocument::segment`] is the only sanctioned way back from that id
//! to `{start_ms, end_ms, speaker, text}`.
//!
//! Two things happen on the way in that are easy to miss and expensive to get
//! wrong:
//!
//! * **Revisions collapse.** `fotw_stt` emits partials sharing an `id` with the
//!   final that supersedes them (spec 7.2 rule 4). Feeding all of them to the
//!   model would show the same utterance three times, once truncated, and give
//!   the validator three different "verbatim" texts to match against.
//! * **The block text is decorated with its own index.** Call B has citations
//!   *off* (spec 8.4) and carries evidence by explicit `evidence_segment_ids`,
//!   so the model can only name an id it can see. The decoration is what makes
//!   the two calls agree on one numbering.

use std::collections::BTreeMap;

use fotw_stt::transcript::{Source, TranscriptSegment};

use crate::tokens::estimate_block_tokens;

/// One transcript segment as it appears in the document, plus everything the
/// UI needs to resolve a citation back to audio.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentSegment {
    /// Position in the document. **This is the evidence id** — the value that
    /// appears in a citation's `start_block_index` and in the model's
    /// `evidence_segment_ids`. Global across map-reduce chunks (spec 8.1).
    pub index: usize,
    /// The originating `TranscriptSegment::id` (a ULID). Kept so a summary can
    /// be re-grounded after the transcript is re-indexed, and so the store's
    /// foreign keys stay meaningful.
    pub segment_id: String,
    /// Normalized speaker label, `None` when the provider reported none.
    pub speaker: Option<String>,
    /// Which capture stream this came from. `Mic` is definitionally the user
    /// (spec 7.5), which is why the rendered label can say "Me" without
    /// diarization having run.
    pub source: Source,
    /// Milliseconds from session t0 on our clock.
    pub start_ms: u64,
    /// Milliseconds from session t0 on our clock.
    pub end_ms: u64,
    /// The spoken text, verbatim. **The evidence validator matches quotes
    /// against this field and nothing else** (spec 8.6 rule 2), so it must
    /// never carry the index/speaker decoration.
    pub text: String,
    /// Segment-level confidence as the provider reported it.
    ///
    /// Load-bearing for spec 8.6's stated gap: a diarization error produces a
    /// correctly-cited action item assigned to the wrong person, and the
    /// mitigation is to refuse to auto-assign an owner from a low-confidence
    /// segment. See [`crate::validate`].
    pub confidence: Option<f64>,
}

impl DocumentSegment {
    /// The text of this segment's content block, decoration included.
    ///
    /// Format: `[#12] S1 @ 04:07 — text`. The pieces, and why each is there:
    ///
    /// * `[#12]` — the evidence id. Call B has no citations API to lean on.
    /// * `S1` — the speaker label the model must copy *exactly* into `owner`
    ///   rather than paraphrase into a first name it invented.
    /// * `@ 04:07` — lets the prose reference times without the model doing
    ///   millisecond arithmetic, which it does badly.
    #[must_use]
    pub fn block_text(&self) -> String {
        format!(
            "[#{}] {} @ {} — {}",
            self.index,
            self.speaker_label(),
            format_timestamp(self.start_ms),
            self.text
        )
    }

    /// The label shown to the model for this segment's speaker.
    ///
    /// Falls back to the capture stream, because "who said it" is structurally
    /// known for the microphone even when diarization is off (spec 7.2 rule 2).
    #[must_use]
    pub fn speaker_label(&self) -> String {
        match (&self.speaker, self.source) {
            (Some(label), _) => label.clone(),
            (None, Source::Mic) => "me".to_string(),
            (None, Source::System) => "unknown".to_string(),
        }
    }
}

/// `mm:ss` for a millisecond offset, growing to `h:mm:ss` past an hour.
fn format_timestamp(ms: u64) -> String {
    let total_seconds = ms / 1_000;
    let (hours, minutes, seconds) = (
        total_seconds / 3_600,
        (total_seconds % 3_600) / 60,
        total_seconds % 60,
    );
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// The transcript, indexed for citation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptDocument {
    segments: Vec<DocumentSegment>,
}

impl TranscriptDocument {
    /// Build the document from raw `fotw-stt` segments.
    ///
    /// Collapses revisions (newest wins per `id`), drops empty text, and sorts
    /// by `start_ms` with the segment ULID as tiebreak. The ULID tiebreak is
    /// not cosmetic: two streams (mic and system) can produce identical
    /// `start_ms`, and an unstable order would mean the same transcript
    /// produced different evidence ids on two runs, which breaks SUM-09's
    /// promise that a stored summary stays resolvable.
    #[must_use]
    pub fn from_segments(segments: &[TranscriptSegment]) -> Self {
        let mut newest: BTreeMap<&str, &TranscriptSegment> = BTreeMap::new();
        for segment in segments {
            newest
                .entry(segment.id.as_str())
                .and_modify(|existing| {
                    if segment.revision >= existing.revision {
                        *existing = segment;
                    }
                })
                .or_insert(segment);
        }

        let mut kept: Vec<&TranscriptSegment> = newest
            .into_values()
            .filter(|segment| !segment.text.trim().is_empty())
            .collect();
        kept.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then_with(|| a.id.cmp(&b.id)));

        let segments = kept
            .into_iter()
            .enumerate()
            .map(|(index, segment)| DocumentSegment {
                index,
                segment_id: segment.id.clone(),
                speaker: segment.speaker.clone(),
                source: segment.source,
                start_ms: segment.start_ms,
                end_ms: segment.end_ms,
                text: segment.text.trim().to_string(),
                confidence: segment.confidence,
            })
            .collect();

        Self { segments }
    }

    /// Every segment, in block order.
    #[must_use]
    pub fn segments(&self) -> &[DocumentSegment] {
        &self.segments
    }

    /// Resolve an evidence id to its segment.
    ///
    /// `None` means the model named an id that does not exist, which spec 8.6
    /// rule 1 says is a drop.
    #[must_use]
    pub fn segment(&self, index: usize) -> Option<&DocumentSegment> {
        self.segments.get(index)
    }

    /// Number of blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Whether the transcript is empty. Legal — SUM-01 says notes are optional
    /// and so, symmetrically, is speech.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Every distinct speaker label in the document.
    ///
    /// The evidence validator's rule 3 (spec 8.6) checks a proposed `owner`
    /// against this set first.
    #[must_use]
    pub fn speaker_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .segments
            .iter()
            .map(DocumentSegment::speaker_label)
            .collect();
        labels.sort();
        labels.dedup();
        labels
    }

    /// The spoken text of the given segments, concatenated in the order given.
    ///
    /// Returns `None` if **any** id is unknown, because spec 8.6 rule 1 drops
    /// the whole item in that case rather than validating against the subset
    /// that happened to exist.
    #[must_use]
    pub fn concatenated_text(&self, indices: &[usize]) -> Option<String> {
        let mut parts = Vec::with_capacity(indices.len());
        for &index in indices {
            parts.push(self.segment(index)?.text.as_str());
        }
        Some(parts.join(" "))
    }

    /// Estimated tokens for the whole transcript as content blocks.
    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| estimate_block_tokens(&segment.block_text()))
            .sum()
    }

    /// The content blocks for a `document` payload, in index order.
    #[must_use]
    pub fn content_blocks(&self) -> Vec<serde_json::Value> {
        self.blocks_for(&self.all_indices())
    }

    /// Content blocks for a subset of segments, in the order given.
    ///
    /// Map-reduce (spec 8.1) sends one chunk at a time; the decoration inside
    /// each block still carries the **global** index, so the model's
    /// `evidence_segment_ids` are global even though the API's own
    /// `start_block_index` is chunk-local. `crate::chunk::Chunk::to_global`
    /// closes the second gap.
    #[must_use]
    pub fn blocks_for(&self, indices: &[usize]) -> Vec<serde_json::Value> {
        indices
            .iter()
            .filter_map(|&index| self.segment(index))
            .map(|segment| serde_json::json!({ "type": "text", "text": segment.block_text() }))
            .collect()
    }

    /// `0..len`, the trivial index list.
    #[must_use]
    pub fn all_indices(&self) -> Vec<usize> {
        (0..self.segments.len()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::segment;

    #[test]
    fn block_index_maps_one_to_one_onto_segments_and_timestamps() {
        // The property spec 8.4 buys with custom-content documents: block N of
        // the document is segment N, whose start_ms is a real audio offset.
        let doc = TranscriptDocument::from_segments(&[
            segment("a", "S0", 0, 1_000, "first"),
            segment("b", "S1", 1_000, 2_500, "second"),
            segment("c", "S0", 2_500, 4_000, "third"),
        ]);

        assert_eq!(doc.len(), 3);
        assert_eq!(doc.content_blocks().len(), 3);
        for (index, expected_start) in [(0, 0), (1, 1_000), (2, 2_500)] {
            let found = doc.segment(index).expect("segment exists");
            assert_eq!(found.index, index);
            assert_eq!(found.start_ms, expected_start);
        }
        assert_eq!(doc.segment(0).expect("first").text, "first");
        assert_eq!(doc.segment(2).expect("third").text, "third");
        assert!(doc.segment(3).is_none(), "id past the end must not resolve");
    }

    #[test]
    fn one_content_block_per_segment_never_merged() {
        let inputs: Vec<_> = (0..25)
            .map(|i| segment(&format!("s{i}"), "S0", i * 100, i * 100 + 90, "hello there"))
            .collect();
        let doc = TranscriptDocument::from_segments(&inputs);
        assert_eq!(doc.content_blocks().len(), 25);
    }

    #[test]
    fn partial_revisions_collapse_to_the_newest() {
        // Spec 7.2 rule 4: partials share an id with the final that supersedes
        // them. Three revisions of one utterance must be one block, not three.
        let mut first = segment("u1", "S0", 0, 500, "we should ship");
        first.revision = 0;
        let mut second = segment("u1", "S0", 0, 900, "we should ship it friday");
        second.revision = 1;
        second.is_final = true;

        let doc = TranscriptDocument::from_segments(&[first, second]);
        assert_eq!(doc.len(), 1);
        assert_eq!(
            doc.segment(0).expect("only").text,
            "we should ship it friday"
        );
    }

    #[test]
    fn empty_segments_never_take_an_index() {
        // An empty block would burn an evidence id that can never be cited and
        // silently shift every id after it.
        let doc = TranscriptDocument::from_segments(&[
            segment("a", "S0", 0, 10, "real"),
            segment("b", "S0", 10, 20, "   "),
            segment("c", "S0", 20, 30, "also real"),
        ]);
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.segment(1).expect("second").text, "also real");
    }

    #[test]
    fn ordering_is_stable_when_two_streams_share_a_start_time() {
        let mut mic = segment("zzz", "me", 5_000, 6_000, "from the mic");
        mic.source = Source::Mic;
        let system = segment("aaa", "S1", 5_000, 6_000, "from the tap");

        let one = TranscriptDocument::from_segments(&[mic.clone(), system.clone()]);
        let other = TranscriptDocument::from_segments(&[system, mic]);
        assert_eq!(
            one, other,
            "index assignment must not depend on input order"
        );
    }

    #[test]
    fn block_text_carries_the_global_index_and_speaker_but_the_raw_text_does_not() {
        let doc = TranscriptDocument::from_segments(&[
            segment("a", "S0", 0, 10, "alpha"),
            segment("b", "S1", 250_000, 251_000, "beta"),
        ]);
        let second = doc.segment(1).expect("second");
        let block = second.block_text();
        assert!(block.starts_with("[#1] S1 @ 04:10 — "), "got {block:?}");
        // The validator matches against `text`; decoration in there would let a
        // model "quote" a timestamp it invented.
        assert_eq!(second.text, "beta");
    }

    #[test]
    fn mic_segments_without_diarization_label_as_me() {
        let mut mic = segment("a", "", 0, 10, "hello");
        mic.speaker = None;
        mic.source = Source::Mic;
        let doc = TranscriptDocument::from_segments(&[mic]);
        assert_eq!(doc.segment(0).expect("only").speaker_label(), "me");
    }

    #[test]
    fn concatenated_text_refuses_an_unknown_id() {
        let doc = TranscriptDocument::from_segments(&[
            segment("a", "S0", 0, 10, "alpha"),
            segment("b", "S1", 10, 20, "beta"),
        ]);
        assert_eq!(
            doc.concatenated_text(&[0, 1]).as_deref(),
            Some("alpha beta")
        );
        assert_eq!(
            doc.concatenated_text(&[1, 0]).as_deref(),
            Some("beta alpha")
        );
        assert!(doc.concatenated_text(&[0, 9]).is_none());
    }

    #[test]
    fn speaker_labels_are_deduped_and_sorted() {
        let doc = TranscriptDocument::from_segments(&[
            segment("a", "S1", 0, 10, "one"),
            segment("b", "S0", 10, 20, "two"),
            segment("c", "S1", 20, 30, "three"),
        ]);
        assert_eq!(
            doc.speaker_labels(),
            vec!["S0".to_string(), "S1".to_string()]
        );
    }

    #[test]
    fn timestamps_grow_a_third_field_past_an_hour() {
        assert_eq!(format_timestamp(0), "00:00");
        assert_eq!(format_timestamp(61_000), "01:01");
        assert_eq!(format_timestamp(3_600_000), "1:00:00");
        assert_eq!(format_timestamp(7_384_000), "2:03:04");
    }

    #[test]
    fn blocks_for_a_chunk_keep_the_global_index_in_their_text() {
        let inputs: Vec<_> = (0..10)
            .map(|i| segment(&format!("s{i}"), "S0", i * 100, i * 100 + 90, "text"))
            .collect();
        let doc = TranscriptDocument::from_segments(&inputs);
        let blocks = doc.blocks_for(&[7, 8, 9]);
        assert_eq!(blocks.len(), 3);
        let first = blocks[0]["text"].as_str().expect("text");
        assert!(
            first.starts_with("[#7]"),
            "chunk-local blocks lost the global id: {first:?}"
        );
    }
}

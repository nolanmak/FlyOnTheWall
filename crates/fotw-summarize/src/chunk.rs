//! Long-meeting strategy (spec 8.1).
//!
//! **Map-reduce is a local-model code path, not the default.** Against a 1M
//! context, a three-hour meeting at 55–75k tokens is under a tenth of the
//! window; engaging map-reduce there would spend several extra calls to produce
//! a worse summary, because the reduce step only ever sees its own chunks'
//! notes and cannot notice that something said in hour one was contradicted in
//! hour three. [`plan`] therefore returns [`Plan::SingleShot`] until the
//! transcript passes `usable_context * 0.6`.
//!
//! When it does fire, three rules from spec 8.1, each of which closes a
//! specific failure:
//!
//! * **Pack whole speaker turns, never splitting an utterance.** A chunk
//!   boundary in the middle of "…so I'll take the migration, but only if" turns
//!   one commitment into two half-claims, and the half in the second chunk has
//!   lost who was speaking.
//! * **Two turns of overlap.** A commitment made in the last turn of a chunk
//!   usually has its owner or its deadline in the turn before or after it.
//! * **Segment ids stay global.** The reduce step and the evidence validator
//!   both work in document coordinates, so a chunk carries the global index of
//!   every segment in it and [`Chunk::to_global`] converts the API's chunk-local
//!   `start_block_index` back.
//!
//! **Do not topic-segment for chunking** (spec 8.1). Topic boundaries are
//! themselves an LLM inference: they add a call, a failure mode and a
//! nondeterminism to a step whose entire job is to be boring. Turn boundaries
//! are already in the data.

use crate::capabilities::Capabilities;
use crate::document::TranscriptDocument;
use crate::tokens::{estimate_block_tokens, estimate_tokens};

/// Turns of overlap between adjacent chunks (spec 8.1).
pub const OVERLAP_TURNS: usize = 2;

/// Ceiling on the running "context so far" block (spec 8.1).
pub const CONTEXT_SO_FAR_BUDGET_TOKENS: usize = 800;

/// A run of consecutive segments from one speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerTurn {
    /// Global segment indices, in order.
    pub segments: Vec<usize>,
    /// The speaker label this turn belongs to.
    pub speaker: String,
    /// Estimated tokens for the whole turn as content blocks.
    pub tokens: usize,
}

/// One unit of the map step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// **Global** segment indices, in order. Chunk-local block index `i`
    /// corresponds to `segments[i]`.
    pub segments: Vec<usize>,
    /// How many leading segments are repeated from the previous chunk.
    ///
    /// The reduce step uses this to avoid double-counting an action item that
    /// was visible to two chunks.
    pub overlap_segments: usize,
    /// Estimated tokens.
    pub tokens: usize,
}

impl Chunk {
    /// Convert a chunk-local block index into a global segment index.
    ///
    /// The Citations API numbers blocks within the document it was sent, which
    /// under map-reduce is one chunk. Without this, every citation from chunk 2
    /// onwards resolves to a segment near the start of the meeting — a bug that
    /// would look like plausible output and be found by a user, not by a test.
    #[must_use]
    pub fn to_global(&self, local_block_index: usize) -> Option<usize> {
        self.segments.get(local_block_index).copied()
    }
}

/// What to do with this transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// One call with the whole transcript (spec 8.1's default).
    SingleShot,
    /// Map over chunks, then reduce.
    MapReduce {
        /// The chunks, in order.
        chunks: Vec<Chunk>,
    },
}

impl Plan {
    /// Whether this plan needs more than one call.
    #[must_use]
    pub fn is_map_reduce(&self) -> bool {
        matches!(self, Self::MapReduce { .. })
    }

    /// The chunks, or a single implicit chunk covering the whole document.
    #[must_use]
    pub fn chunks(&self, document: &TranscriptDocument) -> Vec<Chunk> {
        match self {
            Self::SingleShot => vec![Chunk {
                segments: document.all_indices(),
                overlap_segments: 0,
                tokens: document.estimated_tokens(),
            }],
            Self::MapReduce { chunks } => chunks.clone(),
        }
    }
}

/// Group consecutive same-speaker segments into turns.
///
/// A turn is the atom of chunking: whatever else happens, its segments stay
/// together.
#[must_use]
pub fn speaker_turns(document: &TranscriptDocument) -> Vec<SpeakerTurn> {
    let mut turns: Vec<SpeakerTurn> = Vec::new();
    for segment in document.segments() {
        let speaker = segment.speaker_label();
        let tokens = estimate_block_tokens(&segment.block_text());
        match turns.last_mut() {
            Some(turn) if turn.speaker == speaker => {
                turn.segments.push(segment.index);
                turn.tokens += tokens;
            }
            _ => turns.push(SpeakerTurn {
                segments: vec![segment.index],
                speaker,
                tokens,
            }),
        }
    }
    turns
}

/// Decide how to summarize this transcript (spec 8.1).
#[must_use]
pub fn plan(document: &TranscriptDocument, capabilities: &Capabilities) -> Plan {
    if capabilities.fits_single_shot(document.estimated_tokens()) {
        return Plan::SingleShot;
    }
    Plan::MapReduce {
        chunks: pack(&speaker_turns(document), capabilities.chunk_budget_tokens()),
    }
}

/// Pack turns into chunks of at most `budget` tokens, with overlap.
///
/// A turn larger than the whole budget becomes a chunk on its own rather than
/// being split: spec 8.1 says never split an utterance, and a chunk slightly
/// over budget is a recoverable cost while a severed commitment is not.
#[must_use]
pub fn pack(turns: &[SpeakerTurn], budget: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut previous_end = 0;

    while start < turns.len() {
        let mut end = start;
        let mut tokens = 0;
        while end < turns.len() {
            let turn_tokens = turns[end].tokens;
            // `end > start` keeps an over-budget turn from producing an empty
            // chunk and an infinite loop.
            if end > start && tokens + turn_tokens > budget {
                break;
            }
            tokens += turn_tokens;
            end += 1;
        }

        let segments: Vec<usize> = turns[start..end]
            .iter()
            .flat_map(|turn| turn.segments.iter().copied())
            .collect();
        let overlap_segments = if chunks.is_empty() {
            0
        } else {
            turns[start..previous_end.min(end)]
                .iter()
                .map(|turn| turn.segments.len())
                .sum()
        };
        chunks.push(Chunk {
            segments,
            overlap_segments,
            tokens,
        });

        if end >= turns.len() {
            break;
        }
        previous_end = end;
        // Step back OVERLAP_TURNS, but always make progress.
        start = end.saturating_sub(OVERLAP_TURNS).max(start + 1);
    }

    chunks
}

/// Trim a running "context so far" block to spec 8.1's 800-token ceiling.
///
/// Keeps the **tail**: the reduce step's job is continuity, and what happened
/// most recently is what the next chunk is most likely to refer back to.
#[must_use]
pub fn truncate_context(context: &str, budget_tokens: usize) -> String {
    if estimate_tokens(context) <= budget_tokens {
        return context.to_string();
    }
    let words: Vec<&str> = context.split_whitespace().collect();
    let mut kept: Vec<&str> = Vec::new();
    for word in words.iter().rev() {
        kept.push(word);
        if estimate_tokens(&kept.join(" ")) > budget_tokens {
            kept.pop();
            break;
        }
    }
    kept.reverse();
    kept.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{sample_meeting, segment};

    /// `count` segments, alternating speaker every `run` segments.
    fn transcript(count: usize, run: usize) -> TranscriptDocument {
        let segments: Vec<_> = (0..count)
            .map(|i| {
                let speaker = if (i / run).is_multiple_of(2) {
                    "S0"
                } else {
                    "S1"
                };
                segment(
                    &format!("s{i:04}"),
                    speaker,
                    (i as u64) * 1_000,
                    (i as u64) * 1_000 + 900,
                    "this utterance is exactly ten words long for the test",
                )
            })
            .collect();
        TranscriptDocument::from_segments(&segments)
    }

    fn frontier() -> Capabilities {
        Capabilities::anthropic_frontier()
    }

    #[test]
    fn a_normal_meeting_is_single_shot_on_a_frontier_model() {
        // Spec 8.1: map-reduce is a local-model path. A real meeting against
        // 1M context must not engage it.
        let document = TranscriptDocument::from_segments(&sample_meeting());
        assert_eq!(plan(&document, &frontier()), Plan::SingleShot);

        let three_hours = transcript(2_000, 3);
        assert!(three_hours.estimated_tokens() > 50_000);
        assert_eq!(plan(&three_hours, &frontier()), Plan::SingleShot);
    }

    #[test]
    fn the_switch_happens_exactly_at_sixty_percent_of_usable_context() {
        let document = transcript(200, 2);
        let tokens = document.estimated_tokens();

        // Usable context sized so the transcript is just under 60%.
        let under = Capabilities {
            usable_context_tokens: (tokens * 100) / 59,
            ..frontier()
        };
        assert_eq!(plan(&document, &under), Plan::SingleShot);

        let over = Capabilities {
            usable_context_tokens: (tokens * 100) / 61,
            ..frontier()
        };
        assert!(plan(&document, &over).is_map_reduce());
    }

    #[test]
    fn a_local_model_map_reduces_an_ordinary_meeting() {
        let document = transcript(1_500, 2);
        let plan = plan(&document, &Capabilities::local_default());
        assert!(plan.is_map_reduce());
    }

    #[test]
    fn turns_group_consecutive_segments_from_one_speaker() {
        let document = transcript(9, 3);
        let turns = speaker_turns(&document);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].segments, vec![0, 1, 2]);
        assert_eq!(turns[0].speaker, "S0");
        assert_eq!(turns[1].segments, vec![3, 4, 5]);
        assert_eq!(turns[1].speaker, "S1");
        assert_eq!(turns[2].segments, vec![6, 7, 8]);
    }

    #[test]
    fn a_chunk_never_splits_a_speaker_turn() {
        // Spec 8.1's hard rule. Checked structurally: every turn that appears
        // in a chunk appears in it whole.
        let document = transcript(400, 3);
        let turns = speaker_turns(&document);
        let chunks = pack(&turns, 900);
        assert!(chunks.len() > 3, "the budget must force several chunks");

        for chunk in &chunks {
            for turn in &turns {
                let present: Vec<usize> = turn
                    .segments
                    .iter()
                    .copied()
                    .filter(|index| chunk.segments.contains(index))
                    .collect();
                assert!(
                    present.is_empty() || present == turn.segments,
                    "turn {:?} was split across a chunk boundary (chunk has {present:?})",
                    turn.segments
                );
            }
        }
    }

    #[test]
    fn adjacent_chunks_overlap_by_two_turns() {
        let document = transcript(400, 3);
        let turns = speaker_turns(&document);
        let chunks = pack(&turns, 900);
        assert!(chunks.len() >= 3);

        for pair in chunks.windows(2) {
            let (previous, next) = (&pair[0], &pair[1]);
            let shared: Vec<usize> = next
                .segments
                .iter()
                .copied()
                .filter(|index| previous.segments.contains(index))
                .collect();
            assert!(!shared.is_empty(), "chunks did not overlap at all");
            assert_eq!(
                shared.len(),
                next.overlap_segments,
                "overlap_segments disagrees with the actual overlap"
            );
            // Two turns of three segments each.
            assert_eq!(
                shared.len(),
                OVERLAP_TURNS * 3,
                "expected {OVERLAP_TURNS} turns of overlap"
            );
            // And the overlap is a prefix of the next chunk, not scattered.
            assert_eq!(&next.segments[..shared.len()], shared.as_slice());
        }
    }

    #[test]
    fn every_segment_appears_in_at_least_one_chunk() {
        let document = transcript(400, 3);
        let chunks = pack(&speaker_turns(&document), 900);
        for index in document.all_indices() {
            assert!(
                chunks.iter().any(|chunk| chunk.segments.contains(&index)),
                "segment {index} fell out of every chunk"
            );
        }
    }

    #[test]
    fn segment_ids_stay_global_across_chunks() {
        // The property the reduce step and the validator both depend on: a
        // chunk-local block index resolves back to a document-wide segment.
        let document = transcript(400, 3);
        let chunks = pack(&speaker_turns(&document), 900);
        let last = chunks.last().expect("chunks");

        assert!(
            last.segments[0] > 100,
            "the last chunk restarted its numbering at zero"
        );
        assert_eq!(last.to_global(0), Some(last.segments[0]));
        assert_eq!(last.to_global(1), Some(last.segments[1]));
        assert_eq!(last.to_global(last.segments.len()), None);

        // And a citation from the middle of the last chunk resolves to a
        // segment whose timestamp is late in the meeting, not early.
        let global = last.to_global(2).expect("third block");
        let resolved = document.segment(global).expect("resolves");
        assert!(resolved.start_ms > 100_000);
    }

    #[test]
    fn an_oversized_turn_becomes_its_own_chunk_rather_than_being_split() {
        // Never splitting an utterance wins over the budget. A chunk slightly
        // over budget is recoverable; a severed commitment is not.
        let turns = vec![
            SpeakerTurn {
                segments: vec![0],
                speaker: "S0".to_string(),
                tokens: 50,
            },
            SpeakerTurn {
                segments: vec![1, 2, 3],
                speaker: "S1".to_string(),
                tokens: 5_000,
            },
            SpeakerTurn {
                segments: vec![4],
                speaker: "S0".to_string(),
                tokens: 50,
            },
        ];
        let chunks = pack(&turns, 500);
        let monster = chunks
            .iter()
            .find(|chunk| chunk.segments.contains(&1))
            .expect("the big turn is somewhere");
        assert!(
            [1, 2, 3].iter().all(|i| monster.segments.contains(i)),
            "the oversized turn was split"
        );
    }

    #[test]
    fn packing_terminates_when_every_turn_is_over_budget() {
        // The loop steps back OVERLAP_TURNS each time; without the
        // `max(start + 1)` guard this input never terminates.
        let turns: Vec<SpeakerTurn> = (0..10)
            .map(|i| SpeakerTurn {
                segments: vec![i],
                speaker: format!("S{i}"),
                tokens: 10_000,
            })
            .collect();
        let chunks = pack(&turns, 100);
        assert_eq!(chunks.len(), 10);
        assert_eq!(chunks[0].segments, vec![0]);
        assert_eq!(chunks[9].segments, vec![9]);
    }

    #[test]
    fn an_empty_transcript_produces_no_chunks() {
        assert!(pack(&[], 1_000).is_empty());
    }

    #[test]
    fn single_shot_still_reports_one_chunk_covering_everything() {
        // So the pipeline has one code path for citation remapping instead of
        // two, and the single-shot case is the identity mapping.
        let document = TranscriptDocument::from_segments(&sample_meeting());
        let chunks = Plan::SingleShot.chunks(&document);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].segments, document.all_indices());
        assert_eq!(chunks[0].to_global(3), Some(3));
    }

    #[test]
    fn the_running_context_is_capped_and_keeps_the_tail() {
        let long = (0..2_000)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let trimmed = truncate_context(&long, CONTEXT_SO_FAR_BUDGET_TOKENS);
        assert!(estimate_tokens(&trimmed) <= CONTEXT_SO_FAR_BUDGET_TOKENS);
        assert!(
            trimmed.ends_with("word1999"),
            "truncation dropped the most recent context instead of the oldest"
        );
        assert!(!trimmed.starts_with("word0"));

        let short = "already short enough";
        assert_eq!(truncate_context(short, CONTEXT_SO_FAR_BUDGET_TOKENS), short);
    }
}

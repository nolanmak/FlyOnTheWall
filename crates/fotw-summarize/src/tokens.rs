//! Token estimation for the chunking decision (spec 8.1).
//!
//! **This is an estimate and the spec says so in a warning box.** The 18–25k
//! figure for a one-hour transcript is derived — 150 wpm × ~1.33 tokens/word ×
//! the documented ~30% Claude 4.7+ tokenizer increase — not measured. Spec 8.1
//! asks for it to be refuted or confirmed in M1 by running
//! `POST /v1/messages/count_tokens` against five real transcripts.
//!
//! Two consequences for the code here:
//!
//! 1. The estimator is **deliberately pessimistic**. It rounds up and charges
//!    per-block overhead, because the failure mode of underestimating is a
//!    context-overflow 400 mid-meeting, while the failure mode of
//!    overestimating is an unnecessary map-reduce on a local model.
//! 2. It is a free function over `&str`, not a method on an adapter, so
//!    replacing it with a real tokenizer (or with the count_tokens endpoint's
//!    answer) is a one-file change.

/// Tokens per whitespace-separated word.
///
/// `1.33` tokens/word × the ~30% tokenizer increase documented in spec 8.1.
const TOKENS_PER_WORD: f64 = 1.33 * 1.30;

/// Fixed overhead charged to every content block.
///
/// A block is not just its text: the JSON envelope, the `[#n]` id, the speaker
/// label and the timestamp all cost tokens, and a transcript is thousands of
/// small blocks, so per-block overhead dominates at the margin.
const PER_BLOCK_OVERHEAD_TOKENS: usize = 8;

/// Estimated tokens for a run of text, rounded up.
///
/// Empty and whitespace-only input costs zero; everything else costs at least
/// one token.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    if words == 0 {
        return 0;
    }
    // Word counts never approach f64's exact-integer limit, and the result is
    // ceil'd into a range that fits usize on every target we build for.
    let scaled = (words as f64 * TOKENS_PER_WORD).ceil();
    scaled.max(1.0) as usize
}

/// Estimated tokens for one transcript content block.
///
/// Adds [`PER_BLOCK_OVERHEAD_TOKENS`] to the text's own cost.
#[must_use]
pub fn estimate_block_tokens(text: &str) -> usize {
    estimate_tokens(text) + PER_BLOCK_OVERHEAD_TOKENS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("   \n\t "), 0);
    }

    #[test]
    fn estimate_is_pessimistic_relative_to_word_count() {
        // Never fewer tokens than words: the failure mode of underestimating is
        // a 400 mid-meeting.
        for text in ["one", "one two", "a b c d e f g h i j"] {
            let words = text.split_whitespace().count();
            assert!(
                estimate_tokens(text) >= words,
                "{text:?} estimated below its own word count"
            );
        }
    }

    #[test]
    fn an_hour_of_speech_lands_in_the_spec_8_1_band() {
        // Spec 8.1's derivation, run forwards: 150 wpm for 60 minutes is 9,000
        // words, delivered as roughly 900 ten-word utterances, and the derived
        // answer is 18,000-25,000 tokens for a *diarized, timestamped*
        // transcript.
        //
        // Worth noting what this exposes: 9,000 words at 1.33 x 1.30 is only
        // 15,561 tokens. The spec's band is not reachable from words alone --
        // the "per-utterance overhead" term in its derivation is doing about
        // a third of the work. That is why estimation is per block here and
        // not per transcript, and why a chunker that ignored block overhead
        // would underestimate a long meeting by thousands of tokens.
        let utterance = "this is a ten word utterance for the estimator test";
        assert_eq!(utterance.split_whitespace().count(), 10);
        let estimate: usize = (0..900).map(|_| estimate_block_tokens(utterance)).sum();
        assert!(
            (18_000..=25_000).contains(&estimate),
            "an hour estimated at {estimate} tokens, outside the spec 8.1 band"
        );
    }

    #[test]
    fn block_overhead_is_charged_even_for_a_single_word() {
        assert!(estimate_block_tokens("hi") > estimate_tokens("hi"));
    }
}

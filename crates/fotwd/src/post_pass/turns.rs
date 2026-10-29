//! From two channels of timed words and the system channel's speakers to
//! readable, labelled turns (#246).
//!
//! Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
//! Hari) at commit 97e455794112126a896cae17f5c829e8ffa820c8:
//! native/MuesliNative/Sources/MuesliNativeApp/MicTurnNormalizer.swift (the
//! merge rule: join across a gap of at most 0.35 s, or 1.5 s when either side
//! is a short fragment under 8 visible characters),
//! SystemTurnNormalizer.swift (sentence units as the place to break a long
//! turn) and TranscriptReconciler.swift (preserve first: every mic turn is
//! kept; the system channel is the remote speakers' source of truth; speaker
//! identities are per channel and never shared).
//!
//! Changed: Muesli interpolates times inside a turn by sentence length; here
//! Parakeet's word times place every word, so speakers are assigned per word
//! from the diarizer's segments, and a turn breaks where the speaker changes.

use super::merge::TimedWord;

/// Muesli's `maxMergeGapSeconds`.
pub const MERGE_GAP_MS: u64 = 350;
/// Muesli's `shortSegmentGapCap`.
pub const SHORT_GAP_MS: u64 = 1_500;
/// Muesli's `shortSegmentVisibleLength`.
pub const SHORT_VISIBLE: usize = 8;
/// A turn longer than this breaks at its next sentence end.
pub const LONG_TURN_MS: u64 = 20_000;
/// A word no diarized segment covers takes the nearest one this close.
pub const NEAREST_SPEAKER_MS: u64 = 1_000;

/// The microphone's speaker, as the rest of the library names it.
pub const ME: &str = "me";

/// A diarized stretch of the system channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerSpan {
    /// Start, ms.
    pub start_ms: u64,
    /// End, ms.
    pub end_ms: u64,
    /// Label, stable within the meeting's system channel.
    pub speaker: String,
}

/// Which capture stream a turn came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// This Mac's microphone.
    Mic,
    /// Everyone else, from system audio.
    System,
}

/// One speaker's run of words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The channel.
    pub channel: Channel,
    /// `me` on the mic; a diarized label on the system channel, or `None`
    /// when it has none.
    pub speaker: Option<String>,
    /// First word's start.
    pub start_ms: u64,
    /// Last word's end.
    pub end_ms: u64,
    /// The words, each with its speaker.
    pub words: Vec<(TimedWord, Option<String>)>,
}

impl Turn {
    /// The turn's words joined for display.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (word, _) in &self.words {
            join(&mut out, &word.text);
        }
        out
    }
}

/// Muesli's `joinText`: a space between words unless the next one is
/// punctuation.
fn join(out: &mut String, word: &str) {
    let word = word.trim();
    if word.is_empty() {
        return;
    }
    if !out.is_empty()
        && !word.starts_with(|c: char| c.is_ascii_punctuation() && c != '(' && c != '"')
    {
        out.push(' ');
    }
    out.push_str(word);
}

fn visible(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')'])
        .ends_with(['.', '?', '!'])
}

/// Same-speaker spans this close are one turn of speech. The diarizer works
/// in 10 s windows and its segments often stop ~60 ms short of a window's
/// edge, which would otherwise split a turn there.
pub const SPAN_GAP_MS: u64 = 250;

/// `spans` sorted, with each speaker's spans that touch, overlap or sit
/// within [`SPAN_GAP_MS`] of each other joined.
#[must_use]
pub fn join_spans(mut spans: Vec<SpeakerSpan>) -> Vec<SpeakerSpan> {
    spans.sort_by_key(|s| (s.start_ms, s.end_ms));
    let mut out: Vec<SpeakerSpan> = Vec::new();
    for span in spans {
        // The latest span of the same speaker, if nothing else started since.
        if let Some(last) = out.last_mut()
            && last.speaker == span.speaker
            && span.start_ms <= last.end_ms + SPAN_GAP_MS
        {
            last.end_ms = last.end_ms.max(span.end_ms);
            continue;
        }
        out.push(span);
    }
    out
}

/// The speaker of the system word `[start, end)`: the span overlapping it
/// most, else the nearest within [`NEAREST_SPEAKER_MS`].
#[must_use]
pub fn speaker_at(spans: &[SpeakerSpan], start: u64, end: u64) -> Option<&str> {
    let overlap = |s: &SpeakerSpan| s.end_ms.min(end).saturating_sub(s.start_ms.max(start));
    if let Some(best) = spans
        .iter()
        .filter(|s| overlap(s) > 0)
        .max_by_key(|s| (overlap(s), std::cmp::Reverse(s.start_ms)))
    {
        return Some(&best.speaker);
    }
    let distance = |s: &SpeakerSpan| {
        if s.end_ms <= start {
            start - s.end_ms
        } else {
            s.start_ms.saturating_sub(end)
        }
    };
    spans
        .iter()
        .filter(|s| distance(s) <= NEAREST_SPEAKER_MS)
        .min_by_key(|s| (distance(s), s.start_ms))
        .map(|s| s.speaker.as_str())
}

/// Group one channel's words into turns.
///
/// `other` is the other channel's word starts, sorted: a turn also breaks
/// where someone on the other channel started speaking in its gap, so a
/// back-and-forth reads in the order it was said.
fn channel_turns(
    channel: Channel,
    words: Vec<(TimedWord, Option<String>)>,
    other: &[u64],
) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for (word, speaker) in words {
        if let Some(turn) = turns.last_mut() {
            let from = other.partition_point(|&t| t <= turn.end_ms);
            let interrupted = other.get(from).is_some_and(|&t| t < word.start_ms);
            let gap = word.start_ms.saturating_sub(turn.end_ms);
            let last = &turn.words.last().expect("a turn has a word").0;
            let short =
                visible(&turn.text()) < SHORT_VISIBLE || visible(&word.text) < SHORT_VISIBLE;
            let joins = gap <= MERGE_GAP_MS || (gap <= SHORT_GAP_MS && short);
            let long = turn.end_ms - turn.start_ms >= LONG_TURN_MS && ends_sentence(&last.text);
            if turn.speaker == speaker && joins && !long && !interrupted {
                turn.end_ms = turn.end_ms.max(word.end_ms);
                turn.words.push((word, speaker));
                continue;
            }
        }
        turns.push(Turn {
            channel,
            speaker: speaker.clone(),
            start_ms: word.start_ms,
            end_ms: word.end_ms,
            words: vec![(word, speaker)],
        });
    }
    turns
}

/// Both channels' turns, in time order. Every word is kept (Muesli's
/// preserve-first reconciliation); the cross-channel echo pass the live
/// transcript uses runs on the result afterwards.
///
/// `system_speakers` is `None` when the system channel was not diarized: its
/// turns then carry no speaker, rather than a guessed one.
#[must_use]
pub fn build_turns(
    mic: &[TimedWord],
    system: &[TimedWord],
    system_speakers: Option<&[SpeakerSpan]>,
) -> Vec<Turn> {
    let mic_words = mic
        .iter()
        .map(|w| (w.clone(), Some(ME.to_owned())))
        .collect();
    let mut previous: Option<String> = None;
    let system_words = system
        .iter()
        .map(|w| {
            let speaker = system_speakers.and_then(|spans| {
                speaker_at(spans, w.start_ms, w.end_ms)
                    .map(str::to_owned)
                    .or_else(|| previous.clone())
            });
            previous.clone_from(&speaker);
            (w.clone(), speaker)
        })
        .collect();
    let starts = |words: &[TimedWord]| {
        let mut out: Vec<u64> = words.iter().map(|w| w.start_ms).collect();
        out.sort_unstable();
        out
    };
    let mut turns = channel_turns(Channel::Mic, mic_words, &starts(system));
    turns.extend(channel_turns(Channel::System, system_words, &starts(mic)));
    turns.sort_by_key(|t| (t.start_ms, t.channel == Channel::System, t.end_ms));
    turns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64, end: u64) -> TimedWord {
        TimedWord::new(text, start, end)
    }

    fn span(speaker: &str, start: u64, end: u64) -> SpeakerSpan {
        SpeakerSpan {
            start_ms: start,
            end_ms: end,
            speaker: speaker.into(),
        }
    }

    #[test]
    fn system_words_take_the_diarized_speaker_and_a_turn_breaks_where_it_changes() {
        let system = [
            w("so", 0, 200),
            w("what", 250, 500),
            w("now?", 550, 800),
            w("we", 900, 1_100),
            w("ship.", 1_150, 1_400),
        ];
        let spans = [span("S1", 0, 820), span("S2", 860, 1_500)];
        let turns = build_turns(&[], &system, Some(&spans));
        let shown: Vec<(Option<&str>, String)> = turns
            .iter()
            .map(|t| (t.speaker.as_deref(), t.text()))
            .collect();
        assert_eq!(
            shown,
            [
                (Some("S1"), "so what now?".into()),
                (Some("S2"), "we ship.".into())
            ]
        );
    }

    #[test]
    fn mic_words_are_me_and_channels_interleave_by_time() {
        let mic = [
            w("hi", 0, 300),
            w("there", 350, 600),
            w("yes", 5_000, 5_300),
        ];
        let system = [w("hello", 1_000, 1_400)];
        let turns = build_turns(&mic, &system, None);
        let shown: Vec<(Channel, Option<&str>, String)> = turns
            .iter()
            .map(|t| (t.channel, t.speaker.as_deref(), t.text()))
            .collect();
        assert_eq!(
            shown,
            [
                (Channel::Mic, Some("me"), "hi there".into()),
                (Channel::System, None, "hello".into()),
                (Channel::Mic, Some("me"), "yes".into()),
            ]
        );
    }

    #[test]
    fn muesli_merge_rule_joins_short_fragments_across_longer_gaps() {
        // A 1.2 s gap joins only because one side is a short fragment.
        let mic = [
            w("okay", 0, 300),
            w("let's", 1_500, 1_700),
            w("go", 1_750, 1_900),
        ];
        assert_eq!(build_turns(&mic, &[], None).len(), 1);
        let mic = [
            w("absolutely", 0, 600),
            w("wonderful", 650, 1_100),
            w("everybody", 2_300, 2_800),
            w("agreed", 2_850, 3_200),
        ];
        assert_eq!(build_turns(&mic, &[], None).len(), 2);
    }

    #[test]
    fn a_long_monologue_breaks_at_a_sentence_end() {
        let mic: Vec<TimedWord> = (0..80)
            .map(|i| {
                w(
                    if i % 10 == 9 { "end." } else { "word" },
                    i * 400,
                    i * 400 + 300,
                )
            })
            .collect();
        let turns = build_turns(&mic, &[], None);
        assert!(turns.len() >= 2, "{}", turns.len());
        assert!(
            turns
                .iter()
                .all(|t| t.end_ms - t.start_ms < LONG_TURN_MS + 4_000)
        );
        assert_eq!(turns.iter().map(|t| t.words.len()).sum::<usize>(), 80);
    }

    #[test]
    fn a_turn_breaks_where_the_other_channel_spoke_in_its_gap() {
        // "so what now" on the mic with a 300 ms pause in which the far end
        // says "yes": three turns in spoken order, not one mic turn first.
        let mic = [w("so", 0, 200), w("what", 250, 400), w("now", 700, 900)];
        let system = [w("yes", 450, 650)];
        let turns = build_turns(&mic, &system, None);
        let shown: Vec<String> = turns.iter().map(Turn::text).collect();
        assert_eq!(shown, ["so what", "yes", "now"]);
    }

    #[test]
    fn spans_split_just_short_of_a_window_edge_join_but_other_speakers_do_not() {
        let joined = join_spans(vec![
            span("S1", 2_936, 9_939),
            span("S1", 10_000, 19_939),
            span("S1", 20_000, 23_476),
            span("S2", 23_600, 25_000),
            span("S1", 25_100, 26_000),
            span("S2", 27_000, 28_000),
        ]);
        let shown: Vec<(&str, u64, u64)> = joined
            .iter()
            .map(|s| (s.speaker.as_str(), s.start_ms, s.end_ms))
            .collect();
        assert_eq!(
            shown,
            [
                ("S1", 2_936, 23_476),
                ("S2", 23_600, 25_000),
                ("S1", 25_100, 26_000),
                ("S2", 27_000, 28_000),
            ]
        );
    }

    #[test]
    fn a_word_between_speakers_takes_the_nearest_or_carries_the_last() {
        let spans = [span("S1", 0, 1_000), span("S2", 5_000, 6_000)];
        assert_eq!(speaker_at(&spans, 1_500, 1_700), Some("S1"));
        assert_eq!(speaker_at(&spans, 4_200, 4_400), Some("S2"));
        assert_eq!(speaker_at(&spans, 2_500, 2_700), None);
        let system = [w("a", 500, 700), w("b", 2_500, 2_700)];
        let turns = build_turns(&[], &system, Some(&spans));
        assert!(turns.iter().all(|t| t.speaker.as_deref() == Some("S1")));
    }
}

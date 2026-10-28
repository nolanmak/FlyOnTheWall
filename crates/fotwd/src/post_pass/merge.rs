//! Joining overlapping windows' words without losing or repeating any (#246).
//!
//! Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
//! Hari), native/MuesliNative/Sources/MuesliNativeApp/MeetingRecordingTranscriber.swift
//! (`MeetingRecordingTranscriber.removingOverlap`) at commit
//! 97e455794112126a896cae17f5c829e8ffa820c8.
//!
//! Muesli's rule is kept: **only the short shared boundary is reconciled, never
//! the body of a window.** Its text-only suffix/prefix match is extended with
//! the word times Parakeet reports, because a text match alone duplicates a
//! word the earlier window heard cut off at its edge:
//!
//! 1. A word that touches a window's cut edge was heard truncated there.
//!    The earlier window drops words ending within [`EDGE_GUARD_MS`] of its
//!    end (the next window hears them whole); the later window drops words
//!    starting within it of its start, but only when the earlier window kept
//!    a word over the same time.
//! 2. What is left of the shared stretch is aligned by text: the longest run
//!    (at most [`MAX_MATCH_WORDS`]) that ends the earlier window and starts the
//!    later one, word for word and within [`MATCH_TOLERANCE_MS`] in time, is
//!    kept once.
//! 3. Where the two windows heard the stretch differently, the cut falls at the
//!    middle of the overlap: earlier words before it, later words after it.
//!
//! The merge is a pure function of every window's words, so a job resumed
//! from checkpoints produces exactly what an uninterrupted one does.

use serde::{Deserialize, Serialize};

/// A word with times in milliseconds on the recording's clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedWord {
    /// The word as recognized.
    #[serde(rename = "t")]
    pub text: String,
    /// Start.
    #[serde(rename = "s")]
    pub start_ms: u64,
    /// End.
    #[serde(rename = "e")]
    pub end_ms: u64,
}

impl TimedWord {
    /// A word.
    #[must_use]
    pub fn new(text: impl Into<String>, start_ms: u64, end_ms: u64) -> Self {
        Self {
            text: text.into(),
            start_ms,
            end_ms: end_ms.max(start_ms),
        }
    }

    fn mid(&self) -> u64 {
        self.start_ms + (self.end_ms - self.start_ms) / 2
    }
}

/// One window's words, on the recording's clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowWords {
    /// First sample of the window, in ms.
    pub start_ms: u64,
    /// End of the window, in ms.
    pub end_ms: u64,
    /// What the engine heard, in order.
    pub words: Vec<TimedWord>,
}

/// A word ending (or starting) this close to a cut edge was heard truncated.
pub const EDGE_GUARD_MS: u64 = 250;
/// Muesli's limit on the shared run of words.
pub const MAX_MATCH_WORDS: usize = 8;
/// Two windows' times for the same word agree to within this.
pub const MATCH_TOLERANCE_MS: u64 = 600;

/// Every window's words as one sequence, each spoken word once.
///
/// `windows` must be in order; consecutive windows may overlap.
#[must_use]
pub fn merge_windows(windows: &[WindowWords]) -> Vec<TimedWord> {
    let mut out: Vec<TimedWord> = Vec::new();
    for (k, window) in windows.iter().enumerate() {
        let mut words: Vec<TimedWord> = window
            .words
            .iter()
            .filter(|w| !key(&w.text).is_empty())
            .cloned()
            .collect();
        // Words cut off by this window's end: the next window hears them whole.
        if windows
            .get(k + 1)
            .is_some_and(|next| next.start_ms < window.end_ms)
        {
            words.retain(|w| w.end_ms + EDGE_GUARD_MS < window.end_ms);
        }
        let Some(prev) = k.checked_sub(1).map(|i| &windows[i]) else {
            out.extend(words);
            continue;
        };
        if prev.end_ms <= window.start_ms {
            out.extend(words);
            continue;
        }
        let (overlap_start, overlap_end) = (window.start_ms, prev.end_ms);
        // Words cut off by this window's start, which the earlier one kept.
        words.retain(|w| {
            w.start_ms > overlap_start + EDGE_GUARD_MS
                || !out
                    .iter()
                    .rev()
                    .take_while(|kept| kept.end_ms > overlap_start)
                    .any(|kept| kept.start_ms < w.end_ms && w.start_ms < kept.end_ms)
        });
        let tail_len = out
            .iter()
            .rev()
            .take_while(|w| w.end_ms > overlap_start)
            .count();
        let head_len = words
            .iter()
            .take_while(|w| w.start_ms < overlap_end)
            .count();
        let tail = &out[out.len() - tail_len..];
        let head = &words[..head_len];
        if let Some(n) = shared_run(tail, head) {
            words.drain(..n);
        } else {
            let cut = overlap_start + (overlap_end - overlap_start) / 2;
            let keep_tail = tail.iter().filter(|w| w.mid() < cut).count();
            out.truncate(out.len() - tail_len + keep_tail);
            let drop_head = head.iter().filter(|w| w.mid() < cut).count();
            // `head` is in order, so the words before the cut lead it.
            words.drain(..drop_head);
        }
        out.extend(words);
    }
    out
}

/// The longest run that ends `tail` and starts `head`, word for word.
fn shared_run(tail: &[TimedWord], head: &[TimedWord]) -> Option<usize> {
    let limit = MAX_MATCH_WORDS.min(tail.len()).min(head.len());
    (1..=limit).rev().find(|&n| {
        tail[tail.len() - n..].iter().zip(&head[..n]).all(|(a, b)| {
            key(&a.text) == key(&b.text) && a.start_ms.abs_diff(b.start_ms) <= MATCH_TOLERANCE_MS
        })
    })
}

/// Muesli's comparison key: lowercased, without surrounding punctuation.
fn key(word: &str) -> String {
    word.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace() || c == '…')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 s windows with 2 s of overlap, as the post-pass reads them.
    fn plan(total_ms: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut start = 0;
        loop {
            let end = (start + 30_000).min(total_ms);
            out.push((start, end));
            if end == total_ms {
                return out;
            }
            start = end - 2_000;
        }
    }

    /// What an engine hears of `speech` through each window: every word that
    /// overlaps it, clipped to it, renamed by `truncated` when clipped, with
    /// its times shifted by `jitter(window index)`.
    fn hear(
        speech: &[TimedWord],
        windows: &[(u64, u64)],
        truncated: impl Fn(&str) -> String,
        jitter: impl Fn(usize) -> i64,
    ) -> Vec<WindowWords> {
        windows
            .iter()
            .enumerate()
            .map(|(k, &(start, end))| WindowWords {
                start_ms: start,
                end_ms: end,
                words: speech
                    .iter()
                    .filter(|w| w.start_ms < end && w.end_ms > start)
                    .map(|w| {
                        let clipped = w.start_ms < start || w.end_ms > end;
                        let shift =
                            |t: u64| (t as i64 + jitter(k)).clamp(start as i64, end as i64) as u64;
                        TimedWord::new(
                            if clipped {
                                truncated(&w.text)
                            } else {
                                w.text.clone()
                            },
                            shift(w.start_ms.max(start)),
                            shift(w.end_ms.min(end)),
                        )
                    })
                    .collect(),
            })
            .collect()
    }

    /// Ten minutes of speech: a word every 700 ms, plus a word straddling
    /// every cut edge (both the end of each window and the start of the
    /// next), each word distinct.
    fn straddling_speech(windows: &[(u64, u64)]) -> Vec<TimedWord> {
        let total = windows.last().unwrap().1;
        let mut edges: Vec<u64> = windows
            .windows(2)
            .flat_map(|pair| [pair[1].0, pair[0].1])
            .collect();
        edges.sort_unstable();
        let mut words = Vec::new();
        let mut t = 100;
        let mut n = 0;
        while t + 400 < total {
            if let Some(&edge) = edges.iter().find(|&&e| e >= t && e < t + 1_100) {
                // Straddles `edge`: starts 200 ms before it, ends 250 ms after.
                words.push(TimedWord::new(format!("edge{n}"), edge - 200, edge + 250));
                t = edge + 450;
            } else {
                words.push(TimedWord::new(format!("word{n}"), t, t + 400));
                t += 700;
            }
            n += 1;
        }
        words
    }

    fn texts(words: &[TimedWord]) -> Vec<String> {
        words.iter().map(|w| w.text.clone()).collect()
    }

    #[test]
    fn a_word_straddling_every_window_edge_is_kept_exactly_once() {
        let windows = plan(600_000);
        let speech = straddling_speech(&windows);
        let straddlers = speech.iter().filter(|w| w.text.starts_with("edge")).count();
        assert_eq!(straddlers, 2 * (windows.len() - 1), "one per cut edge");
        let merged = merge_windows(&hear(&speech, &windows, str::to_owned, |_| 0));
        assert_eq!(texts(&merged), texts(&speech));
    }

    #[test]
    fn a_word_misheard_where_it_was_cut_is_still_kept_once() {
        let windows = plan(600_000);
        let speech = straddling_speech(&windows);
        // The engine hears a clipped word as something else entirely.
        let merged = merge_windows(&hear(&speech, &windows, |t| format!("{t}-cut"), |_| 0));
        assert_eq!(texts(&merged), texts(&speech));
    }

    #[test]
    fn windows_that_disagree_slightly_on_times_still_join_cleanly() {
        let windows = plan(600_000);
        let speech = straddling_speech(&windows);
        let jitter = |k: usize| if k % 2 == 0 { 40 } else { -40 };
        let merged = merge_windows(&hear(&speech, &windows, str::to_owned, jitter));
        assert_eq!(texts(&merged), texts(&speech));
    }

    #[test]
    fn repeated_words_in_the_body_of_a_window_are_never_deduplicated() {
        // "no no no" inside one window, and again across the boundary.
        let speech: Vec<TimedWord> = (0..40)
            .map(|i| TimedWord::new("no", 1_000 + i * 1_400, 1_000 + i * 1_400 + 300))
            .collect();
        let windows = plan(60_000);
        let merged = merge_windows(&hear(&speech, &windows, str::to_owned, |_| 0));
        assert_eq!(merged.len(), speech.len());
    }

    #[test]
    fn a_single_window_is_returned_as_heard() {
        let words = vec![
            TimedWord::new("hello", 0, 300),
            TimedWord::new("there", 400, 700),
        ];
        let merged = merge_windows(&[WindowWords {
            start_ms: 0,
            end_ms: 1_000,
            words: words.clone(),
        }]);
        assert_eq!(merged, words);
    }
}

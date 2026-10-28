//! Local post-recognition vocabulary correction.
//!
//! Ported from Muesli, https://github.com/Muesli-HQ/muesli (MIT, © 2026 Pranav
//! Hari), `native/MuesliNative/Sources/MuesliCore/CustomWordMatcher.swift` at
//! commit 97e455794112126a896cae17f5c829e8ffa820c8. See THIRD_PARTY_NOTICES.md.
//!
//! As in Muesli, at each word the longest phrase wins; an exact case-folded
//! match beats a near miss, and a near miss needs Jaro–Winkler similarity at
//! or above the entry's threshold for every token. Boundary punctuation around
//! the phrase is kept; punctuation inside it blocks the match.
//!
//! Differences from Muesli, each pinned by `tests/fixtures/vocabulary-corrections-v1.json`:
//! - Comparison folds accents as well as case (NFD, combining marks removed),
//!   so `jose` matches `José`. Muesli compares canonically equivalent text.
//! - Words are split on any whitespace, which is kept verbatim, not only on
//!   spaces.
//! - A near miss never replaces a frequent English word or its inflection
//!   (`their` is not `Thier`, `scale` is not `Scala`), a token containing a
//!   digit (`H200` is not `H100`), or a longer word containing the entry
//!   (`Adams` is not `Ada`). Its length must be within one character of the
//!   entry's (two below the default threshold), and its first letter the same
//!   or a sound-alike (`Cubectal` → `kubectl`). Exact matches still apply.
//!   Muesli targets small dictionaries; these guards keep a 200-entry profile
//!   from rewriting ordinary prose.
//! - Adjacent words whose letters spell a camel-case entry match it exactly
//!   (`web assembly` → `WebAssembly`, `Fly on the Wall` → `FlyOnTheWall`),
//!   since recognizers often split compounds.
use crate::vocabulary::{
    DEFAULT_MATCHING_THRESHOLD, MAX_MATCHING_THRESHOLD, MIN_MATCHING_THRESHOLD, Profile,
};
use std::collections::{HashMap, HashSet};
use std::{cell::OnceCell, sync::OnceLock};

/// Longest run of words joined to match a compound entry.
const MAX_COMPOUND_WORDS: usize = 6;

#[derive(Clone, Debug)]
struct Entry {
    replacement: String,
    threshold: f64,
    /// Largest length difference a near miss may have.
    max_length_difference: usize,
    tokens: Vec<Vec<char>>,
}

/// Corrections compiled once from a vocabulary profile, then applied per
/// utterance. Apply it only to local-engine text: providers that accept
/// recognition hints already receive them.
#[derive(Clone, Debug, Default)]
pub struct VocabularyMatcher {
    entries: Vec<Entry>,
    /// Entry indices by token count, in profile order.
    by_count: HashMap<usize, Vec<usize>>,
    /// Folded letters of camel-case one-word entries -> first entry index.
    compact: HashMap<String, usize>,
    /// Shortest and longest `compact` key, in characters.
    compact_lengths: Option<(usize, usize)>,
    max_window: usize,
}

impl VocabularyMatcher {
    /// Compile a profile's explicit corrections, then each recognition hint
    /// not already covered by one, as a correction to itself.
    #[must_use]
    pub fn new(profile: &Profile) -> Self {
        let mut matcher = Self::default();
        let mut seen = HashSet::new();
        let explicit = profile.corrections.iter().map(|c| {
            (
                c.word.as_str(),
                c.replacement.as_deref().unwrap_or(&c.word),
                c.matching_threshold.unwrap_or(DEFAULT_MATCHING_THRESHOLD),
            )
        });
        let hints = profile
            .recognition_hints
            .iter()
            .map(|h| (h.as_str(), h.as_str(), DEFAULT_MATCHING_THRESHOLD));
        for (word, replacement, threshold) in explicit.chain(hints) {
            let tokens: Vec<Vec<char>> = word
                .split_whitespace()
                .filter_map(|w| split_punctuation(w).map(|(_, core, _)| fold(core)))
                .collect();
            // A word of combining marks alone folds to nothing.
            if tokens.is_empty() || tokens.iter().any(Vec::is_empty) || !seen.insert(tokens.clone())
            {
                continue;
            }
            let index = matcher.entries.len();
            if tokens.len() == 1 && is_camel_case(word) {
                let length = tokens[0].len();
                let (low, high) = matcher.compact_lengths.unwrap_or((length, length));
                matcher.compact_lengths = Some((low.min(length), high.max(length)));
                matcher
                    .compact
                    .entry(tokens[0].iter().collect())
                    .or_insert(index);
            }
            matcher.max_window = matcher.max_window.max(tokens.len());
            matcher
                .by_count
                .entry(tokens.len())
                .or_default()
                .push(index);
            let threshold = threshold.clamp(MIN_MATCHING_THRESHOLD, MAX_MATCHING_THRESHOLD);
            matcher.entries.push(Entry {
                replacement: replacement.to_owned(),
                threshold,
                max_length_difference: if threshold < DEFAULT_MATCHING_THRESHOLD {
                    2
                } else {
                    1
                },
                tokens,
            });
        }
        if matcher.compact_lengths.is_some() {
            matcher.max_window = matcher.max_window.max(MAX_COMPOUND_WORDS);
        }
        matcher
    }

    /// Number of correction targets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there is nothing to correct.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Correct local-engine text, keeping whitespace and boundary punctuation.
    #[must_use]
    pub fn apply(&self, text: &str) -> String {
        if self.entries.is_empty() || text.is_empty() {
            return text.to_owned();
        }
        let words = words(text);
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut i = 0;
        while i < words.len() {
            let Some((consumed, entry)) = self.best_match(&words[i..]) else {
                i += 1;
                continue;
            };
            let (first, last) = (&words[i], &words[i + consumed - 1]);
            out.push_str(&text[copied..first.core_start]);
            out.push_str(&self.entries[entry].replacement);
            copied = last.core_end;
            i += consumed;
        }
        out.push_str(&text[copied..]);
        out
    }

    fn best_match(&self, words: &[Word]) -> Option<(usize, usize)> {
        for count in (1..=self.max_window.min(words.len())).rev() {
            let window = &words[..count];
            if !keeps_phrase_punctuation(window) {
                continue;
            }
            let entries = self.by_count.get(&count).map_or(&[][..], Vec::as_slice);
            if let Some(&exact) = entries.iter().find(|&&e| {
                self.entries[e]
                    .tokens
                    .iter()
                    .zip(window)
                    .all(|(t, w)| *t == w.folded)
            }) {
                return Some((count, exact));
            }
            let length: usize = window.iter().map(|w| w.folded.len()).sum();
            if count > 1
                && self
                    .compact_lengths
                    .is_some_and(|(low, high)| (low..=high).contains(&length))
            {
                let compact: String = window.iter().flat_map(|w| w.folded.iter()).collect();
                if let Some(&joined) = self.compact.get(&compact) {
                    return Some((count, joined));
                }
            }
            let mut best = None;
            let mut best_score = 0.0;
            for &e in entries {
                if let Some(score) = similarity(window, &self.entries[e])
                    && score > best_score
                {
                    best_score = score;
                    best = Some(e);
                }
            }
            if let Some(e) = best {
                return Some((count, e));
            }
        }
        None
    }
}

/// A whitespace-delimited word and its core between boundary punctuation.
struct Word {
    core_start: usize,
    core_end: usize,
    has_prefix: bool,
    has_suffix: bool,
    folded: Vec<char>,
    has_digit: bool,
    /// Looked up only when a near miss gets that far.
    common: OnceCell<bool>,
}

impl Word {
    fn near_miss_allowed(&self) -> bool {
        !self.has_digit
            && !*self
                .common
                .get_or_init(|| is_common_word(&self.folded.iter().collect::<String>()))
    }
}

fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut offset = 0;
    for raw in text.split(char::is_whitespace) {
        let start = offset;
        offset += raw.len()
            + text[start + raw.len()..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        if raw.is_empty() {
            continue;
        }
        let Some((prefix, core, suffix)) = split_punctuation(raw) else {
            // Punctuation alone can neither match nor be part of a phrase.
            out.push(Word {
                core_start: start,
                core_end: start + raw.len(),
                has_prefix: true,
                has_suffix: true,
                folded: vec![],
                has_digit: false,
                common: OnceCell::new(),
            });
            continue;
        };
        out.push(Word {
            core_start: start + prefix.len(),
            core_end: start + prefix.len() + core.len(),
            has_prefix: !prefix.is_empty(),
            has_suffix: !suffix.is_empty(),
            folded: fold(core),
            has_digit: core.chars().any(char::is_numeric),
            common: OnceCell::new(),
        });
    }
    out
}

/// Muesli's rule: only the phrase's outer edges may carry punctuation.
fn keeps_phrase_punctuation(window: &[Word]) -> bool {
    if window.iter().any(|w| w.folded.is_empty()) {
        return false;
    }
    let last = window.len() - 1;
    window
        .iter()
        .enumerate()
        .all(|(i, w)| (i == 0 || !w.has_prefix) && (i == last || !w.has_suffix))
}

fn similarity(window: &[Word], entry: &Entry) -> Option<f64> {
    let mut total = 0.0;
    for (word, token) in window.iter().zip(&entry.tokens) {
        if word.folded != *token {
            if word.folded.len().abs_diff(token.len()) > entry.max_length_difference
                || !sounds_alike(word.folded[0], token[0])
                || (word.folded.len() > token.len() && contains(&word.folded, token))
                || !word.near_miss_allowed()
            {
                return None;
            }
            let score = jaro_winkler_chars(&word.folded, token);
            if score < entry.threshold {
                return None;
            }
            total += score;
        } else {
            total += 1.0;
        }
    }
    Some(total / entry.tokens.len() as f64)
}

/// Same first letter, or one recognizers commonly confuse with it.
fn sounds_alike(a: char, b: char) -> bool {
    a == b
        || ["ckq", "csz", "gj", "fv", "aeiou"]
            .iter()
            .any(|group| group.contains(a) && group.contains(b))
}

/// A lowercase letter followed by an uppercase one, as in `WebAssembly`.
fn is_camel_case(word: &str) -> bool {
    word.chars()
        .zip(word.chars().skip(1))
        .any(|(a, b)| a.is_lowercase() && b.is_uppercase())
}

fn contains(haystack: &[char], needle: &[char]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_boundary_punctuation(c: char) -> bool {
    matches!(
        c,
        '.' | ','
            | '!'
            | '?'
            | ';'
            | ':'
            | '"'
            | '\''
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '‘'
            | '’'
            | '“'
            | '”'
    )
}

/// Whether a whitespace-free word has something besides boundary punctuation.
pub(crate) fn is_matchable_word(word: &str) -> bool {
    !word.is_empty() && split_punctuation(word).is_some()
}

/// Split boundary punctuation from a word; `None` when nothing else is left.
fn split_punctuation(word: &str) -> Option<(&str, &str, &str)> {
    let core = word.trim_matches(is_boundary_punctuation);
    if core.is_empty() {
        return None;
    }
    let start = word.len() - word.trim_start_matches(is_boundary_punctuation).len();
    let end = start + core.len();
    Some((&word[..start], core, &word[end..]))
}

/// Case- and accent-insensitive comparison form.
fn fold(text: &str) -> Vec<char> {
    let decomposed = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd().normalize(text);
    decomposed
        .chars()
        .filter(|&c| !is_combining_mark(c))
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Jaro–Winkler similarity between two strings, from 0.0 to 1.0, as Muesli
/// computes it (prefix boost of 0.1 per shared leading character, up to 4).
#[must_use]
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    jaro_winkler_chars(&a, &b)
}

fn jaro_winkler_chars(a: &[char], b: &[char]) -> f64 {
    let jaro = jaro(a, b);
    if jaro <= 0.0 {
        return 0.0;
    }
    let prefix = a.iter().zip(b).take(4).take_while(|(x, y)| x == y).count();
    jaro + prefix as f64 * 0.1 * (1.0 - jaro)
}

fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let Some(window) = (a.len().max(b.len()) / 2).checked_sub(1) else {
        return 0.0;
    };
    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0usize;
    for (i, ca) in a.iter().enumerate() {
        let start = i.saturating_sub(window);
        let end = (i + window).min(b.len() - 1);
        if start > end {
            continue;
        }
        for j in start..=end {
            if !b_matched[j] && *ca == b[j] {
                a_matched[i] = true;
                b_matched[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut transpositions = 0usize;
    let mut k = 0;
    for (i, ca) in a.iter().enumerate() {
        if !a_matched[i] {
            continue;
        }
        while !b_matched[k] {
            k += 1;
        }
        if *ca != b[k] {
            transpositions += 1;
        }
        k += 1;
    }
    let m = matches as f64;
    let t = transpositions as f64 / 2.0;
    (m / a.len() as f64 + m / b.len() as f64 + (m - t) / m) / 3.0
}

/// Frequent English words, a hand-written list; see the file's header.
fn common_words() -> &'static HashSet<&'static str> {
    static WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        include_str!("common_words_en.txt")
            .lines()
            .map(|line| line.split('#').next().unwrap_or_default())
            .flat_map(str::split_whitespace)
            .collect()
    })
}

/// Whether a folded word is a frequent English word or a regular inflection
/// of one (`scales`, `called`, `setting`, `easier`).
fn is_common_word(word: &str) -> bool {
    let word = word.replace('’', "'");
    let words = common_words();
    if words.contains(word.as_str()) {
        return true;
    }
    const SUFFIXES: [(&str, &str); 14] = [
        ("'s", ""),
        ("ies", "y"),
        ("ied", "y"),
        ("ier", "y"),
        ("iest", "y"),
        ("ily", "y"),
        ("es", ""),
        ("s", ""),
        ("ed", ""),
        ("ing", ""),
        ("er", ""),
        ("est", ""),
        ("ly", ""),
        ("n", ""),
    ];
    SUFFIXES.iter().any(|(suffix, append)| {
        let Some(stem) = word.strip_suffix(suffix) else {
            return false;
        };
        if stem.len() < 2 {
            return false;
        }
        let stem = format!("{stem}{append}");
        // `settled`/`scaled` drop an `e`; `setting`/`bigger` double a consonant.
        let mut chars = stem.chars().rev();
        let undoubled = match (chars.next(), chars.next()) {
            (Some(a), Some(b)) if a == b => Some(&stem[..stem.len() - a.len_utf8()]),
            _ => None,
        };
        words.contains(stem.as_str())
            || words.contains(format!("{stem}e").as_str())
            || undoubled.is_some_and(|u| words.contains(u))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_words_are_lowercase_and_inflections_resolve() {
        assert!(common_words().len() > 1000);
        assert!(
            common_words()
                .iter()
                .all(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '\''))
        );
        for word in [
            "their", "scale", "scales", "scaled", "scaling", "called", "setting", "sets", "easier",
            "biggest", "builds", "don't", "it’s", "studies", "children",
        ] {
            assert!(is_common_word(word), "{word}");
        }
        for word in [
            "kvex",
            "cubectal",
            "kubernete",
            "telemtry",
            "museli",
            "thier",
        ] {
            assert!(!is_common_word(word), "{word}");
        }
    }

    #[test]
    fn entries_that_fold_to_nothing_are_skipped() {
        let profile = Profile {
            recognition_hints: vec!["\u{301}".into(), "a \u{301}".into()],
            ..Profile::default()
        };
        profile.validate().unwrap();
        let matcher = VocabularyMatcher::new(&profile);
        assert!(matcher.is_empty());
        assert_eq!(matcher.apply("a \u{301} b"), "a \u{301} b");
    }

    // Ported from Muesli's JaroWinklerTests.
    #[test]
    fn jaro_winkler_matches_muesli() {
        assert!((jaro_winkler("hello", "hello") - 1.0).abs() < f64::EPSILON);
        assert!(jaro_winkler("abc", "xyz") < 0.5);
        assert!(jaro_winkler("pranav", "pranab") > 0.85);
        assert!((jaro_winkler("", "") - 1.0).abs() < f64::EPSILON);
        assert!(jaro_winkler("hello", "").abs() < f64::EPSILON);
        assert!(jaro_winkler("", "hello").abs() < f64::EPSILON);
        for (a, b) in [("a", "b"), ("I", "muesli"), ("a", "abcdefghij")] {
            assert!((0.0..=1.0).contains(&jaro_winkler(a, b)), "{a} {b}");
        }
        assert!((jaro_winkler("a", "a") - 1.0).abs() < f64::EPSILON);
        assert!(jaro_winkler("muesli", "muesly") > jaro_winkler("xuesli", "muesly"));
        // Hand-computed: m = 6, no transpositions, no shared prefix.
        let expected = (6.0 / 8.0 + 6.0 / 7.0 + 1.0) / 3.0;
        assert!((jaro_winkler("cubectal", "kubectl") - expected).abs() < 1e-12);
    }
}

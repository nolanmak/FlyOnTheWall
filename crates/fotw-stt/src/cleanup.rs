//! Last-line cleanup of a local model's text (#242).
//!
//! Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
//! Hari) at commit `97e455794112126a896cae17f5c829e8ffa820c8`:
//! `native/MuesliNative/Sources/MuesliNativeApp/TranscriptionEngineArtifactsFilter.swift`
//! (`removeArtifacts` in `TranscriptionRuntime.swift`) and the bracket pattern
//! in `native/MuesliNative/Sources/MuesliCore/InsightsWordAnalyzer.swift`.
//!
//! The speech gate ([`crate::vad`]) is what keeps music and noise away from the
//! model. This is the backstop for what still gets through:
//!
//! - [`remove_artifacts`] strips the annotations Whisper writes instead of
//!   speech — `[BLANK_AUDIO]`, `[music]`, `(applause)`, `<|nospeech|>` —
//!   wherever they sit in a line;
//! - [`is_filler_hallucination`] recognises a segment that is nothing but one
//!   of Whisper's stock phrases ("Thank you.", "Thanks for watching!") *and*
//!   that the model itself doubted was speech. A person who says "thank you"
//!   is heard confidently, so the phrase alone is never enough.

/// A segment whose no-speech probability is at least this, and whose text is
/// only filler, is dropped.
pub const FILLER_NO_SPEECH_THRESHOLD: f32 = 0.3;

/// `text` with model annotations removed and whitespace tidied. Empty when
/// nothing but annotations was there.
#[must_use]
pub fn remove_artifacts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut removed = false;
    let mut rest = text.trim();
    while let Some(open) = rest.find(['[', '(', '<']) {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        match artifact_len(after) {
            Some(len) => {
                out.push(' ');
                removed = true;
                rest = &after[len..];
            }
            None => {
                let c = after.chars().next().map_or(1, char::len_utf8);
                out.push_str(&after[..c]);
                rest = &after[c..];
            }
        }
    }
    out.push_str(rest);

    let mut text = out.as_str().trim_start();
    // A streaming decoder may prefix an annotation with a speaker marker; do
    // not leave the marker behind when the annotation goes.
    if removed {
        text = text.trim_start_matches(['>', '»', ' ']);
    }
    tidy(text)
}

/// Collapse runs of whitespace and drop the space before punctuation.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        let glued = word.starts_with([',', '.', ';', ':', '!', '?']);
        if !out.is_empty() && !glued {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// What Whisper writes in brackets instead of speech. A bracket holding one of
/// these, or one followed by more words (`[music playing]`), is an artifact.
const BRACKETED: &[&str] = &[
    "blank audio",
    "no speech",
    "silence",
    "inaudible",
    "music",
    "applause",
    "laughter",
    "screaming",
    "cheering",
    "audience laughing",
];

/// Decoder tokens written as `<tok>`.
const ANGLED: &[&str] = &["eou", "eob", "unk", "blank", "pad"];

/// The byte length of the artifact `text` starts with, if it starts with one.
fn artifact_len(text: &str) -> Option<usize> {
    let open = text.chars().next()?;
    if let Some(body) = text.strip_prefix("<|") {
        // Any `<|...|>` control token: `<|nospeech|>`, `<|endoftext|>`.
        let end = body.find(['|', '<', '>'])?;
        return body[end..].starts_with("|>").then_some(2 + end + 2);
    }
    let close = match open {
        '[' => ']',
        '(' => ')',
        '<' => '>',
        _ => return None,
    };
    let end = text[1..].find([close, '\n'])? + 1;
    if !text[end..].starts_with(close) {
        return None;
    }
    let inner: String = text[1..end]
        .to_lowercase()
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let foreign = {
        let rest = inner.strip_prefix("speaking in ").unwrap_or(&inner);
        let rest = rest.strip_prefix("a ").unwrap_or(rest);
        rest == "foreign language"
    };
    let artifact = match open {
        '<' => ANGLED.contains(&inner.as_str()) || foreign,
        _ => {
            foreign
                || BRACKETED.iter().any(|word| {
                    inner == *word
                        || (open == '['
                            && inner
                                .strip_prefix(word)
                                .is_some_and(|more| more.starts_with(' ')))
                })
        }
    };
    artifact.then_some(end + close.len_utf8())
}

/// Whether a whole segment is only a sound label — `Music`, `(Music)`,
/// `[Applause]`, `♪` — rather than speech. Only the whole segment counts: a
/// label word inside a sentence is speech.
#[must_use]
pub fn is_sound_label(text: &str) -> bool {
    let notes = ['♪', '♫', '♬', '♩'];
    let label = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        // Nothing but notes is the model's way of writing music.
        return text.contains(notes);
    }
    SOUND_LABELS.contains(&label.as_str())
}

/// What Whisper writes, bracketed or bare, as the whole of a segment over
/// non-speech.
const SOUND_LABELS: &[&str] = &[
    "music",
    "music playing",
    "instrumental music",
    "applause",
    "laughter",
    "cheering",
    "silence",
    "inaudible",
    "blank audio",
    "no speech",
];

/// Whisper's stock phrases over non-speech, normalised: lowercase, letters,
/// digits, apostrophes and single spaces.
const FILLERS: &[&str] = &[
    "thank you",
    "thank you so much",
    "thank you very much",
    "thanks",
    "thanks for watching",
    "thank you for watching",
    "thanks for listening",
    "thank you for listening",
    "please subscribe",
    "like and subscribe",
    "subscribe",
    "bye",
    "bye bye",
    "goodbye",
    "you",
];

/// Whether a segment is a stock phrase the model wrote over non-speech: every
/// sentence in it is one of [`FILLERS`], and the model's own no-speech
/// probability is at least [`FILLER_NO_SPEECH_THRESHOLD`].
#[must_use]
pub fn is_filler_hallucination(text: &str, no_speech_probability: f32) -> bool {
    if no_speech_probability < FILLER_NO_SPEECH_THRESHOLD {
        return false;
    }
    let mut phrases = text
        .split(['.', '!', '?', ',', ';'])
        .map(|phrase| {
            phrase
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '\'')
                .collect::<String>()
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|phrase| !phrase.is_empty())
        .peekable();
    phrases.peek().is_some() && phrases.all(|phrase| FILLERS.contains(&phrase.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filler_is_dropped_only_when_the_model_doubted_it_was_speech() {
        // (text, no-speech probability, dropped?)
        let table: &[(&str, f32, bool)] = &[
            ("Thank you.", 0.0, false),
            ("Thank you.", 0.29, false),
            ("Thank you.", 0.3, true),
            ("Thank you.", 0.9, true),
            ("thank you", 0.5, true),
            (" THANK YOU! ", 0.5, true),
            ("Thank you. Thank you.", 0.4, true),
            ("Thanks for watching!", 0.35, true),
            ("Thanks for watching!", 0.1, false),
            ("Thank you for watching.", 0.6, true),
            ("Bye.", 0.31, true),
            ("Bye.", 0.05, false),
            ("Bye bye.", 0.5, true),
            ("You", 0.8, true),
            ("you", 0.2, false),
            // Real sentences that merely contain or start with filler are kept.
            ("Thank you for the update on the budget.", 0.9, false),
            ("You should ship it.", 0.9, false),
            ("Bye for now, see you Monday.", 0.9, false),
            ("Thanks, Pat.", 0.9, false),
            ("", 0.9, false),
        ];
        for &(text, p, dropped) in table {
            assert_eq!(
                is_filler_hallucination(text, p),
                dropped,
                "{text:?} at no-speech {p}"
            );
        }
    }

    #[test]
    fn a_segment_that_is_only_a_sound_label_is_not_speech() {
        for text in [
            "Music",
            "Music.",
            " music ",
            "(Music)",
            "[Music]",
            "♪",
            "♪ ♪",
            "♪ Music ♪",
            "[MUSIC PLAYING]",
            "Applause.",
            "(laughter)",
        ] {
            assert!(is_sound_label(text), "{text:?}");
        }
        for text in [
            "",
            "Music is my life.",
            "Turn the music down.",
            "The applause was loud.",
            "Thank you.",
            "So, music.",
            "♪ We will rock you ♪",
        ] {
            assert!(!is_sound_label(text), "{text:?}");
        }
    }

    #[test]
    fn artifacts_are_removed_wherever_they_sit() {
        let table: &[(&str, &str)] = &[
            ("[BLANK_AUDIO]", ""),
            ("[blank audio]", ""),
            ("[ Music ]", ""),
            ("(applause)", ""),
            ("[Laughter] So that's the plan.", "So that's the plan."),
            ("So that's the plan. [MUSIC]", "So that's the plan."),
            ("Hello [inaudible] there", "Hello there"),
            ("<|nospeech|>", ""),
            ("<|endoftext|> Yes.", "Yes."),
            ("(speaking in foreign language)", ""),
            ("[Speaking in a foreign language]", ""),
            ("<unk> okay", "okay"),
            (">> [no speech]", ""),
            ("Hello , world", "Hello, world"),
            // Ordinary brackets that are not annotations survive.
            ("Use the array [0] here.", "Use the array [0] here."),
            ("(laughs) yes", "(laughs) yes"),
            ("We hit 42% (up from 30%).", "We hit 42% (up from 30%)."),
        ];
        for &(text, expected) in table {
            assert_eq!(remove_artifacts(text), expected, "{text:?}");
        }
    }
}

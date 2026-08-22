//! Transcribing the mic leg too — issue #60.
//!
//! Both legs are captured, but every transcript so far was built from the
//! system leg alone: your own voice was recorded and never appeared in the
//! text. `TranscriptSegment` already carries `source`, the speaker normalizer
//! already labels an undiarized mic stream `me`, and the wire config already
//! turns diarization off for `Source::Mic` — the missing piece is a per-leg
//! `Transcription` shape and the plumbing behind it.
//!
//! # The cost decision, pinned here
//!
//! A second stream doubles the Deepgram bill, which is why spec 7.5 made it
//! an explicit decision rather than a default. The decision this file
//! encodes: **on by default**, because a meeting-notes tool that omits the
//! note-taker's half of every conversation fails at its one job — and off by
//! an explicit `FOTW_MIC_STT=off`, because a bill is a thing a user must be
//! able to decline.

use fotw_stt::Source;
use fotw_stt::transcript::{TimestampSource, TranscriptSegment};
use fotwd::session::{DeepgramLegs, mic_stt_enabled, order_segments};

// ------------------------------------------------------------- the opt-out

#[test]
fn mic_transcription_is_on_when_nobody_said_otherwise() {
    assert!(mic_stt_enabled(None));
}

#[test]
fn the_documented_spelling_turns_it_off() {
    assert!(!mic_stt_enabled(Some("off")));
}

/// `0` and `false` are what muscle memory types; refusing them would turn an
/// attempt to decline a bill into a silent doubling of it.
#[test]
fn the_obvious_spellings_work_too() {
    assert!(!mic_stt_enabled(Some("0")));
    assert!(!mic_stt_enabled(Some("false")));
    assert!(!mic_stt_enabled(Some("OFF")));
}

/// Anything else is not an opt-out. `FOTW_MIC_STT=on` set by a script must
/// not read as off, and a typo must fail toward the documented default.
#[test]
fn other_values_leave_it_on() {
    assert!(mic_stt_enabled(Some("on")));
    assert!(mic_stt_enabled(Some("1")));
    assert!(mic_stt_enabled(Some("")));
    assert!(mic_stt_enabled(Some("no")));
}

// ------------------------------------------------------------- the legs

const KEY: &str = "not-a-real-key";
const SESSION: &str = "fotw-test-session";

#[test]
fn the_system_leg_is_always_transcribed() {
    let legs = DeepgramLegs::for_session(KEY, SESSION, true, true);
    assert_eq!(legs.system.normalizer.source, Source::System);
    assert_eq!(legs.system.normalizer.session_id, SESSION);
    assert_eq!(legs.system.api_key, KEY);
}

#[test]
fn the_mic_leg_exists_when_a_mic_does_and_nobody_opted_out() {
    let legs = DeepgramLegs::for_session(KEY, SESSION, true, true);
    let mic = legs.mic.expect("a machine with a mic gets a mic stream");

    assert_eq!(mic.normalizer.source, Source::Mic);
    assert_eq!(mic.normalizer.session_id, SESSION);
    // Pinned by fotw-stt's own tests too, but asserted here because this is
    // the constructor the daemon actually calls: the mic is one known person,
    // and diarizing it would cost money to un-know that.
    assert!(!mic.normalizer.diarization_enabled);
    assert!(!mic.params.to_query().contains("diarize=true"));
}

/// A machine with no input device still transcribes the far end rather than
/// refusing to start — the same shape as capture itself.
#[test]
fn no_mic_means_no_mic_leg_and_a_working_system_leg() {
    let legs = DeepgramLegs::for_session(KEY, SESSION, false, true);
    assert!(legs.mic.is_none());
    assert_eq!(legs.system.normalizer.source, Source::System);
}

#[test]
fn the_opt_out_removes_the_mic_leg_even_with_a_mic_present() {
    let legs = DeepgramLegs::for_session(KEY, SESSION, true, false);
    assert!(legs.mic.is_none());
}

// ------------------------------------------------------------- the merge

fn seg(source: Source, start_ms: u64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("{start_ms}-{text}"),
        session_id: SESSION.to_owned(),
        source,
        speaker: Some(match source {
            Source::Mic => "me".to_owned(),
            Source::System => "S0".to_owned(),
        }),
        text: text.to_owned(),
        start_ms,
        end_ms: start_ms + 1_000,
        words: Vec::new(),
        confidence: None,
        language: None,
        is_final: true,
        revision: 0,
        provider: "deepgram".to_owned(),
        model: "nova-3".to_owned(),
        timestamp_source: TimestampSource::Provider,
    }
}

/// Two streams finalize independently, so segments arrive interleaved by
/// network luck, not by when the words were said. The transcript must read in
/// spoken order or a two-person exchange renders as two monologues.
#[test]
fn segments_from_both_legs_come_out_in_spoken_order() {
    let mut segments = vec![
        seg(Source::System, 5_000, "and that is why"),
        seg(Source::Mic, 1_000, "wait, before you start"),
        seg(Source::System, 0, "let me share my screen"),
        seg(Source::Mic, 9_000, "makes sense"),
    ];
    order_segments(&mut segments);

    let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "let me share my screen",
            "wait, before you start",
            "and that is why",
            "makes sense",
        ]
    );
}

/// Equal start times keep their arrival order — a stable sort, so reruns of
/// the same meeting produce the same transcript byte for byte.
#[test]
fn a_tie_does_not_reshuffle() {
    let mut segments = vec![
        seg(Source::System, 2_000, "first arrival"),
        seg(Source::Mic, 2_000, "second arrival"),
    ];
    order_segments(&mut segments);
    assert_eq!(segments[0].text, "first arrival");
    assert_eq!(segments[1].text, "second arrival");
}

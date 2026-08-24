//! Cross-leg transcript dedupe — the user's own fix, verbatim: "if we are
//! capturing both system audio and mic can't we just compare both and throw
//! away the delta?"
//!
//! On speakers, the mic re-transcribes the system audio: the same passage
//! lands twice, once diarized on the system leg and once labeled `me`, with
//! ASR wording drift between the copies and multi-second skew between the
//! legs' clocks. The audio-domain gate (CAP-11 v1) catches the
//! single-dominant-source case; this text-level pass removes what leaks,
//! because in the text domain the duplication is trivially visible no matter
//! what the room did to the waveform. Precedent: Descript's "mic bleed" fix —
//! text-only removal, audio untouched.
//!
//! # Where the threshold comes from
//!
//! From the pairs below, which are REAL: captured live from a podcast played
//! over speakers, mic leg vs system leg, drift and all. The margin test runs
//! them against genuinely independent controls and requires the threshold to
//! sit in open water between the populations. If scoring drifts toward the
//! knife edge, this suite fails.

use fotw_stt::Source;
use fotw_stt::transcript::{TimestampSource, TranscriptSegment};
use fotwd::session::{dedupe_cross_leg, text_dedupe_enabled};

const SESSION: &str = "fotw-dedupe-test";

fn seg(source: Source, start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("{start_ms}-{}", text.len()),
        session_id: SESSION.to_owned(),
        source,
        speaker: Some(match source {
            Source::Mic => "me".to_owned(),
            Source::System => "S0".to_owned(),
        }),
        text: text.to_owned(),
        start_ms,
        end_ms,
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

fn texts(segments: &[TranscriptSegment]) -> Vec<(&'static str, String)> {
    segments
        .iter()
        .map(|s| {
            (
                match s.source {
                    Source::Mic => "mic",
                    Source::System => "system",
                },
                s.text.clone(),
            )
        })
        .collect()
}

// ---------------------------------------------------- the real pairs, live

/// The observed drifted pair with numbers: raw token containment is 0.667
/// ("fifteen nine" vs "29"), which no safe threshold reaches. Number-classing
/// inside the matcher lifts it to 0.833. Both copies here are verbatim from
/// the live capture.
#[test]
fn the_number_drifted_pair_is_dropped_via_number_classing() {
    let mut segments = vec![
        seg(
            Source::System,
            10_000,
            16_000,
            "who retired from the UFC undefeated with a perfect 29 and o record",
        ),
        seg(
            Source::Mic,
            12_000,
            18_000,
            "who retires from the UFC undefeated with a perfect fifteen nine and o record",
        ),
        // The corroborating second match every real echo session has.
        seg(
            Source::System,
            20_000,
            24_000,
            "it was a huge honor for me to meet him and to train together",
        ),
        seg(
            Source::Mic,
            21_500,
            25_500,
            "it was a huge honor for me to meet him to train together",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);

    assert_eq!(dropped, 2, "both mic echoes drop: {:?}", texts(&segments));
    assert!(segments.iter().all(|s| s.source == Source::System));
}

/// Verbatim from the live Steve Jobs capture: one long mic blob whose words
/// span THREE system segments, with the observed skew. The union across the
/// padded window has to cover it.
#[test]
fn one_mic_blob_spanning_three_system_segments_is_dropped() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            5_000,
            "Steve didn't care at all It was irrelevant your criticism of what he wanted to \
             He was going to make it happen",
        ),
        seg(
            Source::System,
            5_000,
            11_000,
            "And it's what happens at the very end of this meeting This guy named Paul walks \
             up to them He owns a small computer shop",
        ),
        seg(
            Source::System,
            11_000,
            16_000,
            "the owner of the Byte Shop computer store introduced himself to Steven Woz after \
             the presentation",
        ),
        seg(
            Source::Mic,
            2_000,
            17_500,
            "Steve didn't care at all It was irrelevant Your criticism of what he wanted to do \
             He was going to make it happen And it's what happens at the very end of this this \
             meeting this guy named Paul walks up to them He owns a small com computer shop the \
             owner of the Bite Shop computer store introduced himself to Steven Woz after the",
        ),
        // Corroboration.
        seg(
            Source::System,
            20_000,
            23_000,
            "he goes from prototype the night before to twenty five thousand dollars in sales",
        ),
        seg(
            Source::Mic,
            21_000,
            24_000,
            "he goes from prototype the 94 to 25000 dollars in sales",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);

    assert_eq!(dropped, 2, "{:?}", texts(&segments));
    assert!(segments.iter().all(|s| s.source == Source::System));
}

// ----------------------------------------------------------- what survives

/// The user answering while the far end talks: their words are their own,
/// and no amount of window union may eat them.
#[test]
fn independent_mic_speech_during_playback_is_kept() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            6_000,
            "his obsessive commitment to creating products of style practicality and great \
             consumer appeal",
        ),
        seg(
            Source::Mic,
            2_000,
            7_000,
            "hey can you grab the charger from the kitchen before the call starts",
        ),
        seg(
            Source::System,
            8_000,
            12_000,
            "and his reliance on gut instinct rather than consumer research",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);

    assert_eq!(dropped, 0, "{:?}", texts(&segments));
    assert_eq!(segments.len(), 3);
}

/// A mixed segment where the user's own words dominate is kept — the
/// echo-dominant inverse is the documented accepted loss (#72's subtraction
/// territory, not this pass's).
#[test]
fn a_mic_segment_mostly_the_users_own_words_is_kept() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            4_000,
            "most people never pick up the phone and call",
        ),
        seg(
            Source::Mic,
            1_000,
            8_000,
            "most people never pick up the phone yeah I actually did that last week when I \
             cold called the vendor about our contract renewal and it worked",
        ),
        // Corroborating echo elsewhere so the guard is not what saves it.
        seg(
            Source::System,
            10_000,
            14_000,
            "you gotta act and you gotta be willing to fail",
        ),
        seg(
            Source::Mic,
            11_000,
            15_000,
            "you gotta act and you gotta be willing to fail",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);

    assert_eq!(dropped, 1, "{:?}", texts(&segments));
    assert!(
        segments
            .iter()
            .any(|s| s.source == Source::Mic && s.text.contains("vendor")),
        "the user's mixed utterance was eaten"
    );
}

/// Headphones: no system leg at all. Untouched, no panic.
#[test]
fn a_headphone_session_is_untouched() {
    let mut segments = vec![
        seg(
            Source::Mic,
            0,
            3_000,
            "let me walk you through the rollout plan",
        ),
        seg(
            Source::Mic,
            4_000,
            7_000,
            "first we ship the interconnect changes on Friday",
        ),
    ];
    assert_eq!(dedupe_cross_leg(&mut segments), 0);
    assert_eq!(segments.len(), 2);
}

/// System segments are never candidates, whatever the mic says.
#[test]
fn system_segments_are_never_dropped() {
    let mut segments = vec![
        seg(Source::System, 0, 3_000, "the same words on both legs"),
        seg(Source::Mic, 500, 3_500, "the same words on both legs"),
        seg(Source::System, 5_000, 8_000, "the same words on both legs"),
        seg(Source::Mic, 5_500, 8_500, "the same words on both legs"),
    ];
    dedupe_cross_leg(&mut segments);
    assert_eq!(
        segments
            .iter()
            .filter(|s| s.source == Source::System)
            .count(),
        2
    );
}

// ------------------------------------------------- the corroboration guard

/// One moderate match in a whole session is a human repeating something —
/// echo is never a one-off. Kept.
#[test]
fn a_single_moderate_match_in_a_session_is_kept() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            4_000,
            "the budget review moves to Tuesday at ten",
        ),
        // The user reads it back with drift — moderate containment, alone.
        seg(
            Source::Mic,
            5_000,
            8_000,
            "okay so the budget review moves to Tuesday at ten right",
        ),
        seg(
            Source::System,
            20_000,
            24_000,
            "and Marcus owns the rollout plan going forward",
        ),
        seg(
            Source::Mic,
            30_000,
            33_000,
            "got it I'll sync with him tomorrow",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);
    assert_eq!(dropped, 0, "{:?}", texts(&segments));
}

/// A lone match that is near-verbatim (≥ 0.85 on its own) is echo even
/// without corroboration.
#[test]
fn a_lone_near_verbatim_match_is_dropped() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            5_000,
            "he was a free thinker whose ideas would often run against the conventional wisdom \
             of any community in which he operated",
        ),
        seg(
            Source::Mic,
            1_500,
            6_500,
            "he was a free thinker whose ideas would often run against the conventional wisdom \
             of any community in which he operated",
        ),
        seg(
            Source::Mic,
            20_000,
            23_000,
            "totally unrelated words from the user here",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);
    assert_eq!(dropped, 1, "{:?}", texts(&segments));
}

// ------------------------------------------------------ short segments

/// A short echo fragment — the gate-warmup residue — duplicates its source
/// contiguously and overlaps it tightly. Dropped by the subsequence rule.
#[test]
fn a_short_contiguous_echo_fragment_is_dropped() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            4_000,
            "Barefoot He walks into the byte shop and does a sale",
        ),
        seg(Source::Mic, 800, 2_000, "Barefoot He walks"),
        // Corroboration for the session.
        seg(
            Source::System,
            10_000,
            13_000,
            "Steve said yes first and then learned how to later",
        ),
        seg(
            Source::Mic,
            11_000,
            14_000,
            "Steve said yes first and then learn how to later",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);
    assert_eq!(dropped, 2, "{:?}", texts(&segments));
}

/// A short confirmation spoken AFTER the far end finished — outside the
/// tight short-segment window — is the user's own and is kept.
#[test]
fn a_late_short_confirmation_is_kept() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            3_000,
            "so the total comes to twenty nine fifty",
        ),
        // 6s after the system segment ended: a human confirming, not a room.
        seg(Source::Mic, 9_000, 10_000, "twenty nine fifty"),
        // Unrelated corroborating echo elsewhere must not change this.
        seg(
            Source::System,
            20_000,
            24_000,
            "we will send the invoice over tomorrow morning",
        ),
        seg(
            Source::Mic,
            21_000,
            25_000,
            "we will send the invoice over tomorrow morning",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);
    assert_eq!(dropped, 1, "{:?}", texts(&segments));
    assert!(
        segments
            .iter()
            .any(|s| s.source == Source::Mic && s.text == "twenty nine fifty")
    );
}

// --------------------------------------------------------------- windows

/// The same phrase legitimately repeated much later sits outside the ±10s
/// window and is kept.
#[test]
fn a_repeat_outside_the_window_is_kept() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            4_000,
            "first you believe and then you work on making other people believe too",
        ),
        // The user quotes it back a minute later.
        seg(
            Source::Mic,
            60_000,
            64_000,
            "first you believe and then you work on making other people believe too",
        ),
        seg(
            Source::System,
            70_000,
            74_000,
            "he tirelessly navigated the valley's network of experts",
        ),
        seg(
            Source::Mic,
            71_000,
            75_000,
            "he tirelessly navigated the valley's network of experts",
        ),
    ];
    let dropped = dedupe_cross_leg(&mut segments);
    assert_eq!(dropped, 1, "{:?}", texts(&segments));
    assert!(
        segments
            .iter()
            .any(|s| s.source == Source::Mic && s.start_ms == 60_000)
    );
}

// ------------------------------------------------------------ degenerate

#[test]
fn empty_and_punctuation_only_segments_never_drop_or_panic() {
    let mut segments = vec![
        seg(Source::System, 0, 2_000, "real words on the system leg"),
        seg(Source::Mic, 500, 1_000, ""),
        seg(Source::Mic, 1_200, 1_400, "— …"),
    ];
    assert_eq!(dedupe_cross_leg(&mut segments), 0);
    assert_eq!(segments.len(), 3);
}

#[test]
fn output_stays_in_spoken_order() {
    let mut segments = vec![
        seg(
            Source::System,
            0,
            3_000,
            "alpha bravo charlie delta echo foxtrot",
        ),
        seg(
            Source::Mic,
            1_000,
            4_000,
            "alpha bravo charlie delta echo foxtrot",
        ),
        seg(
            Source::Mic,
            5_000,
            8_000,
            "the user's own remark stays right here",
        ),
        seg(
            Source::System,
            9_000,
            12_000,
            "golf hotel india juliett kilo lima",
        ),
        seg(
            Source::Mic,
            10_000,
            13_000,
            "golf hotel india juliett kilo lima",
        ),
    ];
    dedupe_cross_leg(&mut segments);
    let starts: Vec<u64> = segments.iter().map(|s| s.start_ms).collect();
    let mut sorted = starts.clone();
    sorted.sort_unstable();
    assert_eq!(starts, sorted);
}

// ------------------------------------------------------------ kill switch

#[test]
fn the_kill_switch_spellings_match_the_house_convention() {
    assert!(text_dedupe_enabled(None));
    assert!(!text_dedupe_enabled(Some("off")));
    assert!(!text_dedupe_enabled(Some("0")));
    assert!(text_dedupe_enabled(Some("on")));
}

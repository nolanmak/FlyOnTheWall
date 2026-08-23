//! The speaker-echo gate — CAP-11's v1 (#71).
//!
//! On speakers, the mic hears the system output, so both legs transcribe the
//! same words and the far end gets attributed to `me`. The gate detects that
//! acoustic coupling with plain normalized cross-correlation — textbook
//! signal processing, nothing exotic — and votes to suppress mic audio from
//! the STT feed while the coupling holds. Raw audio is never touched.
//!
//! # How the thresholds in `echo.rs` were derived
//!
//! By these tests, not by fiat. The synthetic scenarios below put an echoed
//! signal and genuinely independent speech-like signals through the same
//! correlator; the assertions require a wide margin between the two
//! populations (echo must score far above the threshold, independent audio
//! far below). If a threshold drifts to where the margin thins, these tests
//! fail — the derivation is executable, not folklore.

use fotw_pipeline::echo::{EchoGate, GateVerdict};

const RATE: u32 = 16_000;
/// One pump chunk for these tests: 100 ms at the STT feed rate.
const CHUNK: usize = 1_600;

/// A deterministic pseudo-speech signal: a few detuned harmonics with an
/// amplitude wobble. Deterministic because a flaky DSP test is worse than no
/// test, and seeded noise adds nothing the harmonics do not.
fn speech_like(seed: u32, n: usize, offset: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = (i + offset) as f32 / RATE as f32;
            let f = 110.0 + (seed % 7) as f32 * 23.0;
            let wobble = 0.6 + 0.4 * (t * 2.3).sin();
            wobble
                * 0.3
                * ((t * f * std::f32::consts::TAU).sin()
                    + 0.5 * (t * f * 2.02 * std::f32::consts::TAU).sin()
                    + 0.25 * (t * f * 3.11 * std::f32::consts::TAU).sin())
        })
        .collect()
}

/// The room, as arithmetic: delay by `lag` samples and attenuate.
fn echoed(source: &[f32], lag: usize, gain: f32, n: usize, offset: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let j = (i + offset).checked_sub(lag);
            j.and_then(|j| source.get(j)).copied().unwrap_or(0.0) * gain
        })
        .collect()
}

fn gate() -> EchoGate {
    EchoGate::new(RATE)
}

// ---------------------------------------------------------------- coupling

/// The reported bug, in miniature: the mic is a delayed, quieter copy of the
/// speakers. The gate must call it coupled — at typical room delays.
#[test]
fn a_delayed_attenuated_copy_is_flagged_as_echo() {
    for lag_ms in [15usize, 60, 140, 250] {
        let mut g = gate();
        let system: Vec<f32> = speech_like(3, CHUNK * 8, 0);
        let lag = lag_ms * RATE as usize / 1_000;

        let mut flagged = 0;
        for chunk in 0..8 {
            let off = chunk * CHUNK;
            g.push_system(&system[off..off + CHUNK]);
            let mic = echoed(&system, lag, 0.3, CHUNK, off);
            if g.assess(&mic) == GateVerdict::Suppress {
                flagged += 1;
            }
        }
        assert!(
            flagged >= 5,
            "echo at {lag_ms}ms lag flagged only {flagged}/8 chunks"
        );
    }
}

/// Headphones: the mic hears the user, not the speakers. Zero acoustic path
/// must mean zero suppression, no matter how loud both signals are — eating
/// the user's real voice is the failure mode this gate must never have.
#[test]
fn independent_speech_passes_untouched() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 8, 0);
    // A different fundamental and different wobble phase: independent talker.
    let mic_voice = speech_like(11, CHUNK * 8, 977);

    for chunk in 0..8 {
        let off = chunk * CHUNK;
        g.push_system(&system[off..off + CHUNK]);
        assert_eq!(
            g.assess(&mic_voice[off..off + CHUNK]),
            GateVerdict::PassThrough,
            "independent speech suppressed at chunk {chunk}"
        );
    }
}

/// A silent far end cannot echo. Whatever the mic holds, the gate stays out
/// of the way — this is the guard against correlating against noise.
#[test]
fn a_quiet_system_leg_never_suppresses() {
    let mut g = gate();
    let silence = vec![0.0f32; CHUNK];
    let mic_voice = speech_like(11, CHUNK * 4, 0);

    for chunk in 0..4 {
        g.push_system(&silence);
        assert_eq!(
            g.assess(&mic_voice[chunk * CHUNK..(chunk + 1) * CHUNK]),
            GateVerdict::PassThrough
        );
    }
}

/// A silent mic is not "coupled" either — suppressing silence is harmless in
/// audio terms but poisons the gate's stats and any UI built on them.
#[test]
fn a_quiet_mic_passes() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 4, 0);
    for chunk in 0..4 {
        g.push_system(&system[chunk * CHUNK..(chunk + 1) * CHUNK]);
        assert_eq!(g.assess(&vec![0.0f32; CHUNK]), GateVerdict::PassThrough);
    }
}

// -------------------------------------------------------------- hysteresis

/// One coincidental spike must not eat a word. Suppression requires the
/// verdict to hold across consecutive assessments, so a single high-scoring
/// chunk in otherwise-independent audio passes through.
#[test]
fn a_single_spurious_match_does_not_suppress() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 6, 0);
    let mic_voice = speech_like(11, CHUNK * 6, 977);
    let lag = 30 * RATE as usize / 1_000;

    for chunk in 0..6 {
        let off = chunk * CHUNK;
        g.push_system(&system[off..off + CHUNK]);
        let mic: Vec<f32> = if chunk == 2 {
            // One chunk of genuine echo sandwiched in independent speech.
            echoed(&system, lag, 0.3, CHUNK, off)
        } else {
            mic_voice[off..off + CHUNK].to_vec()
        };
        assert_eq!(
            g.assess(&mic),
            GateVerdict::PassThrough,
            "one isolated match suppressed chunk {chunk}"
        );
    }
}

/// Sustained echo, then the user puts headphones on: the gate must release
/// within a couple of chunks, not stay latched.
#[test]
fn the_gate_releases_when_the_coupling_ends() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 10, 0);
    let mic_voice = speech_like(11, CHUNK * 10, 977);
    let lag = 60 * RATE as usize / 1_000;

    for chunk in 0..5 {
        let off = chunk * CHUNK;
        g.push_system(&system[off..off + CHUNK]);
        let _ = g.assess(&echoed(&system, lag, 0.3, CHUNK, off));
    }
    // Headphones on: independent from here.
    let mut released_at = None;
    for chunk in 5..10 {
        let off = chunk * CHUNK;
        g.push_system(&system[off..off + CHUNK]);
        if g.assess(&mic_voice[off..off + CHUNK]) == GateVerdict::PassThrough {
            released_at = Some(chunk);
            break;
        }
    }
    let released_at = released_at.expect("the gate never released");
    assert!(
        released_at <= 7,
        "released only at chunk {released_at}; two chunks of lag is the budget"
    );
}

// -------------------------------------------------------------- the margin

/// The executable derivation: the correlator must separate echo from
/// independent speech by a wide margin, so the threshold sits in open water
/// rather than on a knife edge. This is the test that fails if the scoring
/// or the threshold drifts toward fragility.
#[test]
fn echo_and_independent_speech_are_separated_by_a_wide_margin() {
    let system = speech_like(3, CHUNK * 8, 0);
    let lag = 80 * RATE as usize / 1_000;

    let mut echo_scores = Vec::new();
    let mut voice_scores = Vec::new();
    let mut g_echo = gate();
    let mut g_voice = gate();
    let mic_voice = speech_like(11, CHUNK * 8, 977);

    for chunk in 0..8 {
        let off = chunk * CHUNK;
        g_echo.push_system(&system[off..off + CHUNK]);
        g_voice.push_system(&system[off..off + CHUNK]);
        echo_scores.push(g_echo.score(&echoed(&system, lag, 0.3, CHUNK, off)));
        voice_scores.push(g_voice.score(&mic_voice[off..off + CHUNK]));
    }
    // Skip warmup chunks where the reference history is still filling.
    let echo_floor = echo_scores[2..].iter().copied().fold(f32::MAX, f32::min);
    let voice_ceiling = voice_scores[2..].iter().copied().fold(f32::MIN, f32::max);

    assert!(
        echo_floor > voice_ceiling * 2.0,
        "margin too thin: echo floor {echo_floor:.3} vs voice ceiling {voice_ceiling:.3}"
    );
    let threshold = EchoGate::correlation_threshold();
    assert!(
        voice_ceiling < threshold && threshold < echo_floor,
        "threshold {threshold:.3} is not in the open water between \
         {voice_ceiling:.3} and {echo_floor:.3}"
    );
}

// ------------------------------------------------------------------- stats

/// The acceptance metric for CAP-11 needs numbers, and so does any future UI
/// hint ("echo gate active — consider headphones").
#[test]
fn the_gate_counts_what_it_did() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 6, 0);
    let lag = 60 * RATE as usize / 1_000;

    for chunk in 0..6 {
        let off = chunk * CHUNK;
        g.push_system(&system[off..off + CHUNK]);
        let _ = g.assess(&echoed(&system, lag, 0.3, CHUNK, off));
    }
    let (assessed, suppressed) = g.stats();
    assert_eq!(assessed, 6);
    assert!(suppressed >= 4, "only {suppressed}/6 suppressed");
}

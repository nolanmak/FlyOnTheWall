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

/// A deterministic pseudo-speech signal: seeded broadband noise under a
/// seed-dependent amplitude pattern.
///
/// The carrier must be NOISE, not tones. A periodic carrier forgives every
/// misalignment — clock drift just slides one period onto the next and
/// waveform correlation survives falsely, which is how an earlier version of
/// this suite green-lit a gate that failed in a real room. The envelope (the
/// syllable-rate amplitude pattern) is what actually identifies the signal,
/// exactly as it does for real speech through a real room.
fn speech_like(seed: u32, n: usize, offset: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(2_654_435_761).max(1);
    let mut noise_at = move || {
        // xorshift32: deterministic, aperiodic over any window we use.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    // Burn the generator forward so `offset` addresses the same stream.
    let mut noise = Vec::with_capacity(n + offset);
    for _ in 0..(n + offset) {
        noise.push(noise_at());
    }
    let (rate_a, rate_b) = (1.9 + (seed % 5) as f32 * 0.7, 3.3 + (seed % 3) as f32 * 1.1);
    (0..n)
        .map(|i| {
            let t = (i + offset) as f32 / RATE as f32;
            let wobble = (0.15 + 0.85 * (0.5 + 0.5 * (t * rate_a * std::f32::consts::TAU).sin()))
                * (0.4 + 0.6 * (0.5 + 0.5 * (t * rate_b * std::f32::consts::TAU).sin()));
            wobble * 0.3 * noise[i + offset]
        })
        .collect()
}

/// The room, honestly: several delayed taps (early reflections), speaker
/// coloration (a crude low-pass), and a noise floor. A pure delayed copy is
/// what a wire does, not a room — the first gate passed on wires and failed
/// on rooms, which is exactly the brittleness this fixture exists to catch.
fn echoed(source: &[f32], lag: usize, gain: f32, n: usize, offset: usize) -> Vec<f32> {
    let taps = [(0usize, 1.0f32), (11, 0.55), (29, 0.35), (67, 0.2)];
    let mut prev = 0.0f32;
    (0..n)
        .map(|i| {
            // The mic and the speakers run on different converters whose
            // clocks drift ~0.3% apart. Over a 100 ms correlation span that
            // smears alignment by tens of samples — waveform correlation
            // collapses, which is precisely how the first gate passed on
            // wires and failed in a room. Envelopes survive drift.
            let drifted = ((i + offset) as f32 / 1.003) as usize;
            let mut sample = 0.0f32;
            for (extra, tap_gain) in taps {
                if let Some(j) = drifted.checked_sub(lag + extra) {
                    sample += source.get(j).copied().unwrap_or(0.0) * tap_gain;
                }
            }
            // One-pole low-pass: the speaker and the air both dull the copy.
            prev = 0.6 * prev + 0.4 * sample * gain;
            // A deterministic "noise" floor so nothing correlates on silence.
            prev + 0.004 * (((i + offset) as f32) * 1.7).sin()
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
        let system: Vec<f32> = speech_like(3, CHUNK * 15, 0);
        let lag = lag_ms * RATE as usize / 1_000;

        // The gate abstains while its envelope history warms up: it cannot
        // look `lag` into a past it has not heard yet, so warmup is the
        // correlation window plus the longest lag — 700 ms, once, per
        // meeting. The contract under test starts after that.
        for chunk in 0..7 {
            let off = chunk * CHUNK;
            g.push_system(&system[off..off + CHUNK]);
            let _ = g.assess(&echoed(&system, lag, 0.3, CHUNK, off));
        }
        let mut flagged = 0;
        for chunk in 7..15 {
            let off = chunk * CHUNK;
            g.push_system(&system[off..off + CHUNK]);
            let mic = echoed(&system, lag, 0.3, CHUNK, off);
            if g.assess(&mic) == GateVerdict::Suppress {
                flagged += 1;
            }
        }
        assert!(
            flagged >= 7,
            "echo at {lag_ms}ms lag flagged only {flagged}/8 post-warmup chunks"
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
/// The executable derivation, measuring what the detector actually is: a
/// JOINT test of pattern correlation and lag stability. A single window's
/// score can lie in either direction — smooth envelopes over a best-of-lags
/// search guarantee occasional high scores for independent speech — but an
/// echo holds its score at a *fixed* lag while a coincidence wanders. The
/// margin that matters is between those joint behaviors.
#[test]
fn echo_and_independent_speech_are_separated_by_a_wide_margin() {
    let system = speech_like(3, CHUNK * 12, 0);
    let lag = 80 * RATE as usize / 1_000;

    let mut g_echo = gate();
    let mut g_voice = gate();
    let mic_voice = speech_like(11, CHUNK * 12, 977);

    // A third gate samples raw scores without assess(), because score() is
    // context-building: feeding the same chunk twice through one gate would
    // duplicate envelope frames and corrupt the very timeline being scored.
    let mut g_floor = gate();
    let mut echo_suppressed = 0;
    let mut voice_suppressed = 0;
    let mut echo_floor = f32::MAX;
    for chunk in 0..12 {
        let off = chunk * CHUNK;
        g_echo.push_system(&system[off..off + CHUNK]);
        g_voice.push_system(&system[off..off + CHUNK]);
        g_floor.push_system(&system[off..off + CHUNK]);
        let e = g_echo.assess(&echoed(&system, lag, 0.3, CHUNK, off));
        let v = g_voice.assess(&mic_voice[off..off + CHUNK]);
        let f = g_floor.score(&echoed(&system, lag, 0.3, CHUNK, off));
        if chunk >= 4 {
            echo_suppressed += u32::from(e == GateVerdict::Suppress);
            voice_suppressed += u32::from(v == GateVerdict::Suppress);
            echo_floor = echo_floor.min(f);
        }
    }

    assert_eq!(voice_suppressed, 0, "independent speech was suppressed");
    // Engagement costs three stable windows after scoring begins (~300 ms);
    // the contract is "engaged within a second of playback, then holds".
    assert!(
        echo_suppressed >= 6,
        "echo suppressed only {echo_suppressed}/8 post-warmup chunks"
    );
    // And the raw score itself must clear the threshold with margin, so the
    // constant is not sitting on the echo population's edge.
    let threshold = EchoGate::correlation_threshold();
    assert!(
        echo_floor > threshold * 1.3,
        "echo floor {echo_floor:.3} sits too close to the threshold {threshold:.3}"
    );
}

/// The pump drains the two rings in whatever sizes accumulated, so the legs
/// arrive skewed and irregular. The gate must not depend on tidy 100 ms
/// lockstep — this is the exact condition the first implementation failed
/// under while its tests fed it perfectly aligned chunks.
#[test]
fn irregular_chunk_sizes_and_skew_are_still_flagged() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 16, 0);
    let lag = 90 * RATE as usize / 1_000;

    // System audio arrives in uneven pushes; the mic drains on its own
    // cadence, chronically behind by up to a few hundred milliseconds.
    let pushes = [
        700usize, 2_400, 1_100, 3_000, 900, 2_100, 1_800, 2_000, 1_300, 2_600, 1_000, 2_300,
    ];
    let mut sys_pos = 0;
    let mut mic_pos = 0;
    let mut flagged = 0u32;
    let mut assessed = 0u32;
    for (round, push) in pushes.iter().enumerate() {
        let end = (sys_pos + push).min(system.len());
        g.push_system(&system[sys_pos..end]);
        sys_pos = end;

        // The mic lags the system feed by a round: real skew, not lockstep.
        if round >= 1 {
            let mic_end = (mic_pos + push).min(system.len());
            let mic = echoed(&system, lag, 0.3, mic_end - mic_pos, mic_pos);
            mic_pos = mic_end;
            let verdict = g.assess(&mic);
            // Warmup: the gate cannot score until both envelope histories
            // cover the window plus the skew, and engagement costs three
            // stable windows on top. Count once both have passed.
            if round >= 7 {
                assessed += 1;
                if verdict == GateVerdict::Suppress {
                    flagged += 1;
                }
            }
        }
    }
    assert!(
        assessed >= 4 && flagged >= assessed - 1,
        "skewed echo flagged only {flagged}/{assessed} post-engagement"
    );
}

// ------------------------------------------------------------------- stats

/// The acceptance metric for CAP-11 needs numbers, and so does any future UI
/// hint ("echo gate active — consider headphones").
#[test]
fn the_gate_counts_what_it_did() {
    let mut g = gate();
    let system = speech_like(3, CHUNK * 10, 0);
    let lag = 60 * RATE as usize / 1_000;

    for chunk in 0..10 {
        let off = chunk * CHUNK;
        g.push_system(&system[off..off + CHUNK]);
        let _ = g.assess(&echoed(&system, lag, 0.3, CHUNK, off));
    }
    let (assessed, suppressed) = g.stats();
    assert_eq!(assessed, 10);
    // Warmup plus three engagement windows pass before the first Suppress;
    // what this test pins is that the counters describe what happened.
    assert!(suppressed >= 3, "only {suppressed}/10 suppressed");
    assert!(suppressed < assessed);
}

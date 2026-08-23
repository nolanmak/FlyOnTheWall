//! The speaker-echo gate — CAP-11's v1 (#71).
//!
//! # The problem, and the boundary of this fix
//!
//! On speakers, the microphone hears the system output: everything played is
//! captured twice, transcribed twice, and the far end's words land on the
//! leg that is *defined* as the user. This gate detects that acoustic
//! coupling and votes to withhold mic audio from the transcription feed
//! while the coupling holds.
//!
//! It is detection, not cancellation: a frame is either the user's or judged
//! to be mostly the speakers', and talking *over* playback still loses the
//! overlapped words on the mic leg. Subtraction — echo cancellation proper,
//! with the system tap as the far-end reference — is CAP-11's v2 (#72).
//! What v1 buys today: no duplicated transcript on speakers, no `me` label
//! on the far end's words, and no paying the provider to transcribe an echo.
//!
//! # How it decides
//!
//! Textbook normalized cross-correlation. The gate keeps a short history of
//! the system leg (the reference) and scores each mic chunk against it over
//! a range of lags covering device latency plus the acoustic path. A copy of
//! the reference scores near its attenuation-independent maximum regardless
//! of volume; independent speech scores near zero. Two guards keep the
//! obvious failure modes out:
//!
//! * **Headphones**: no acoustic path means low correlation, and low
//!   correlation means the gate does nothing. The one unforgivable failure
//!   is eating the user's real voice.
//! * **Silence**: a quiet reference or a quiet mic cannot vote — correlating
//!   noise floors against each other produces garbage confidence.
//!
//! Suppression also requires the verdict to *persist* across consecutive
//! chunks, so one coincidental spike never costs a word, and release after
//! the coupling ends takes at most a couple of chunks.
//!
//! # Where the numbers come from
//!
//! `tests/echo_gate.rs` derives them: it scores synthetic echo and
//! genuinely independent speech through this very correlator and asserts the
//! threshold sits in the wide margin between the two populations. The
//! derivation is executable — if scoring or threshold drifts toward the
//! knife edge, the suite fails.

/// What to do with one mic chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Feed it to transcription: it is (or may be) the user.
    PassThrough,
    /// Withhold it from transcription: it is judged to be the speakers.
    /// Raw audio is unaffected — the WAL writes before this gate exists.
    Suppress,
}

/// Correlation score above which a chunk reads as coupled.
///
/// Derived by `tests/echo_gate.rs::echo_and_independent_speech_are_separated_
/// by_a_wide_margin`, which requires this to sit between the independent-
/// speech ceiling and the echo floor with room to spare.
const CORRELATION_THRESHOLD: f32 = 0.5;

/// RMS below which a signal is treated as silent, in linear full-scale.
///
/// Roughly -46 dBFS: comfortably above resampler noise, comfortably below
/// any real speech. Both legs must clear it before a chunk may vote.
const ACTIVITY_FLOOR: f32 = 0.005;

/// How far back the reference history reaches, in milliseconds.
///
/// Output-device latency plus a domestic acoustic path plus input latency
/// lands well under 300 ms; the search covers the full window.
const MAX_LAG_MS: usize = 300;

/// Coarse lag step for the search, in milliseconds. A refinement pass around
/// the coarse peak recovers the precision the stride gives up.
const LAG_STRIDE_MS: usize = 3;

/// How much of a mic chunk is scored, in samples at the feed rate.
///
/// 100 ms of speech is plenty to correlate on, and bounding it keeps the
/// per-chunk cost flat no matter how much audio the pump hands over at once.
const SCORE_SPAN: usize = 1_600;

/// Consecutive coupled verdicts required before suppression engages.
///
/// Two: a single chunk is a coincidence, two in a row at 100 ms chunks is a
/// quarter-second of sustained coupling. Release is symmetric — one clean
/// chunk ends the streak, so headphones going on frees the mic within a
/// chunk or two. `tests/echo_gate.rs` pins both behaviors.
const ENGAGE_STREAK: u32 = 2;

/// Detects acoustic coupling between the mic and the system leg.
///
/// Operates on the transcription feed — mono, one shared sample rate, after
/// resampling — because that is the only audio it guards. The WAL path never
/// routes through here.
pub struct EchoGate {
    sample_rate: u32,
    /// The reference: recent system-leg audio, oldest first.
    reference: Vec<f32>,
    /// How many samples the reference retains.
    capacity: usize,
    coupled_streak: u32,
    assessed: u64,
    suppressed: u64,
}

impl EchoGate {
    /// A gate for feeds at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let max_lag = MAX_LAG_MS * sample_rate as usize / 1_000;
        Self {
            sample_rate,
            reference: Vec::new(),
            // Enough history to score a full span at the maximum lag.
            capacity: max_lag + SCORE_SPAN * 2,
            coupled_streak: 0,
            assessed: 0,
            suppressed: 0,
        }
    }

    /// The threshold, exposed so the derivation test can assert it sits in
    /// the margin its own measurements establish.
    #[must_use]
    pub fn correlation_threshold() -> f32 {
        CORRELATION_THRESHOLD
    }

    /// Feed system-leg audio into the reference history.
    pub fn push_system(&mut self, samples: &[f32]) {
        self.reference.extend_from_slice(samples);
        if self.reference.len() > self.capacity {
            let excess = self.reference.len() - self.capacity;
            self.reference.drain(..excess);
        }
    }

    /// Score one mic chunk against the reference: the peak normalized
    /// cross-correlation over the lag search, in `[0, 1]`.
    #[must_use]
    pub fn score(&self, mic: &[f32]) -> f32 {
        let span = mic.len().min(SCORE_SPAN);
        if span < 256 || self.reference.len() < span {
            return 0.0;
        }
        let mic = &mic[..span];
        if rms(mic) < ACTIVITY_FLOOR {
            return 0.0;
        }

        let stride = (LAG_STRIDE_MS * self.sample_rate as usize / 1_000).max(1);
        let max_lag = (MAX_LAG_MS * self.sample_rate as usize / 1_000)
            .min(self.reference.len().saturating_sub(span));

        // Coarse sweep, then refine one stride around the winner.
        let mut best = (0usize, 0.0f32);
        let mut lag = 0;
        while lag <= max_lag {
            let c = self.correlation_at(mic, span, lag);
            if c > best.1 {
                best = (lag, c);
            }
            lag += stride;
        }
        let from = best.0.saturating_sub(stride);
        let to = (best.0 + stride).min(max_lag);
        for lag in from..=to {
            let c = self.correlation_at(mic, span, lag);
            if c > best.1 {
                best = (lag, c);
            }
        }
        best.1
    }

    /// Judge one mic chunk. Call once per chunk, after the same chunk's
    /// system audio was pushed — the reference must not run behind the mic.
    pub fn assess(&mut self, mic: &[f32]) -> GateVerdict {
        self.assessed += 1;

        // A quiet reference cannot echo; a quiet mic is not worth a vote.
        let reference_active = self.reference.len() >= SCORE_SPAN
            && rms(tail(&self.reference, SCORE_SPAN)) >= ACTIVITY_FLOOR;
        let coupled = reference_active && self.score(mic) >= CORRELATION_THRESHOLD;

        if coupled {
            self.coupled_streak = self.coupled_streak.saturating_add(1);
        } else {
            self.coupled_streak = 0;
        }

        if self.coupled_streak >= ENGAGE_STREAK {
            self.suppressed += 1;
            GateVerdict::Suppress
        } else {
            GateVerdict::PassThrough
        }
    }

    /// `(chunks assessed, chunks suppressed)` — CAP-11's acceptance metric,
    /// and the number any "consider headphones" hint would be built on.
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.assessed, self.suppressed)
    }

    /// Normalized cross-correlation of `mic[..span]` against the reference
    /// slice ending `lag` samples before the reference's newest sample.
    fn correlation_at(&self, mic: &[f32], span: usize, lag: usize) -> f32 {
        let end = self.reference.len() - lag;
        let Some(start) = end.checked_sub(span) else {
            return 0.0;
        };
        let reference = &self.reference[start..end];

        let (mut dot, mut mic_sq, mut ref_sq) = (0.0f64, 0.0f64, 0.0f64);
        for (m, r) in mic.iter().zip(reference) {
            dot += f64::from(*m) * f64::from(*r);
            mic_sq += f64::from(*m) * f64::from(*m);
            ref_sq += f64::from(*r) * f64::from(*r);
        }
        let denom = (mic_sq * ref_sq).sqrt();
        if denom <= f64::EPSILON {
            return 0.0;
        }
        // The magnitude is what signals a copy; a phase-inverted echo is
        // still an echo.
        ((dot / denom).abs()) as f32
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    ((sum / samples.len() as f64).sqrt()) as f32
}

fn tail(v: &[f32], n: usize) -> &[f32] {
    &v[v.len().saturating_sub(n)..]
}

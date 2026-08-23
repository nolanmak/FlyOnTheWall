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
//! It is detection, not cancellation: a chunk is either the user's or judged
//! to be mostly the speakers', and talking *over* playback still loses the
//! overlapped words on the mic leg. Subtraction — echo cancellation proper,
//! with the system tap as the far-end reference — is CAP-11's v2 (#72).
//!
//! # Why envelopes, not waveforms
//!
//! The first version of this gate correlated raw waveforms, passed a test
//! suite full of clean delayed copies, and did nothing in a real room. A
//! room does not hand the mic a copy: the speaker colors the signal, the
//! walls smear it across reflections, and the mic and speakers run on
//! different converter clocks that drift apart — a fraction of a percent is
//! enough to slide a 100 ms waveform window out of alignment entirely.
//! Waveform correlation dies under any of these.
//!
//! What survives the room is the **energy envelope**: the syllable-rate
//! loudness pattern of speech. The gate tracks both legs as log-energy
//! envelopes at 10 ms resolution and correlates those, mean-removed, over a
//! lag window wide enough to cover the acoustic path *and* the pump's
//! chunk-to-chunk skew between the legs. Log energy makes attenuation an
//! additive constant, and mean removal deletes constants — so the score
//! measures pattern, not volume.
//!
//! # The guards
//!
//! * **Headphones**: no acoustic path, no envelope match, no suppression.
//!   The one unforgivable failure is eating the user's real voice.
//! * **Silence**: a quiet reference or a quiet mic cannot vote.
//! * **Persistence**: suppression engages only after consecutive coupled
//!   verdicts and releases on the first clean one.
//!
//! # Where the numbers come from
//!
//! `tests/echo_gate.rs` derives them against a simulated *room* — multi-tap
//! reflections, speaker coloration, converter drift, broadband carrier —
//! and asserts the threshold sits in a wide margin between echoed and
//! independent speech. The derivation is executable; drift toward the knife
//! edge fails the suite.

/// What to do with one mic chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Feed it to transcription: it is (or may be) the user.
    PassThrough,
    /// Withhold it from transcription: it is judged to be the speakers.
    /// Raw audio is unaffected — the WAL writes before this gate exists.
    Suppress,
}

/// Onset-pattern correlation above which a chunk reads as coupled.
///
/// Derived by the margin test, which requires this to sit between the
/// independent-speech ceiling and the echo floor with room to spare.
const CORRELATION_THRESHOLD: f32 = 0.7;

/// How close to the round's peak the previous round's lag must still score
/// for the coupling to count as stable.
///
/// Population measurements on the room fixture show why a floor alone
/// cannot work: with a ±3 s signed search, independent speech finds a peak
/// above any workable floor in most windows — a wide search has a high
/// extreme-value baseline for anything. What it cannot fake is the peak
/// STAYING at one lag: echo keeps its old lag within a couple of percent of
/// the maximum round after round, while a coincidence's old lag decays as
/// the peak teleports. This ratio is the boundary between those behaviors.
const STABILITY_RATIO: f32 = 0.92;

/// How much the best lag may wander between consecutive coupled verdicts,
/// in frames. A room's delay is a property of furniture — it holds still.
/// A coincidental alignment between two independent speakers wanders, and
/// this is what disqualifies it even when one window correlates.
const LAG_JITTER_FRAMES: usize = 3;

/// Mean window RMS below which a leg is treated as silent (linear
/// full-scale; ≈ −46 dBFS). Both legs must clear it before a vote counts.
const ACTIVITY_FLOOR: f32 = 0.005;

/// Envelope frame length in milliseconds. Syllables live at 3–8 Hz; 10 ms
/// frames oversample that comfortably while staying cheap.
const FRAME_MS: usize = 10;

/// How many envelope frames the correlation window spans (400 ms). Long
/// enough that a syllable pattern is identity, short enough that release
/// after the coupling ends happens within a couple of chunks.
const WINDOW_FRAMES: usize = 40;

/// Maximum envelope lag searched, in frames each direction (3 s signed).
///
/// The acoustic path itself is tens of milliseconds; the reason for a range
/// two orders larger — and *signed* — is the feed clocks. Live capture
/// showed the system leg delivering the same words seconds after the mic
/// heard them through the speakers: the reference arrived AFTER the echo.
/// A search that only looks backward in reference history can never match
/// that, so the gate slides both ways — mic-now against reference history,
/// and reference-now against mic history.
const MAX_LAG_FRAMES: usize = 300;

/// Frames of mic history required before any verdict. Below this the gate
/// abstains — an unwarmed detector must fail open, never closed.
const WARMUP_FRAMES: usize = 25;

/// Consecutive stable coupled verdicts before suppression engages.
///
/// Three, not one: the population measurements in the derivation test show
/// independent speech spiking past any workable threshold in roughly a third
/// of windows — the score alone cannot separate the populations. What
/// separates them is persistence at a fixed lag: real echo holds its score
/// at its delay round after round, a coincidence cannot hold three in a row.
const ENGAGE_STREAK: u32 = 3;

/// Floor added inside the log so silence has a defined loudness.
const LOG_FLOOR: f32 = 1e-6;

/// One scoring pass over the signed lag search.
#[derive(Debug, Default)]
struct Scored {
    /// The peak correlation anywhere in the search.
    best: f32,
    /// Where the peak was. Positive: the reference is older than the mic
    /// (the normal direction). Negative: the mic heard it first — the
    /// reference feed is running behind.
    best_lag: Option<i64>,
    /// The best correlation in the neighbourhood of the previous round's
    /// lag — the number the stability check actually wants.
    at_previous: f32,
}

/// Accumulates arbitrary-size sample pushes into fixed envelope frames.
struct EnvelopeTracker {
    frame_len: usize,
    /// Sum of squares for the frame being filled.
    acc: f64,
    filled: usize,
    /// Log-energy per completed frame, oldest first, bounded.
    frames: Vec<f32>,
    capacity: usize,
}

impl EnvelopeTracker {
    fn new(frame_len: usize, capacity: usize) -> Self {
        Self {
            frame_len,
            acc: 0.0,
            filled: 0,
            frames: Vec::new(),
            capacity,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        for s in samples {
            self.acc += f64::from(*s) * f64::from(*s);
            self.filled += 1;
            if self.filled == self.frame_len {
                let mean_sq = (self.acc / self.frame_len as f64) as f32;
                self.frames.push((mean_sq + LOG_FLOOR).ln());
                self.acc = 0.0;
                self.filled = 0;
                if self.frames.len() > self.capacity {
                    let excess = self.frames.len() - self.capacity;
                    self.frames.drain(..excess);
                }
            }
        }
    }
}

/// Detects acoustic coupling between the mic and the system leg.
///
/// Operates on the transcription feed — mono, one shared sample rate, after
/// resampling — because that is the only audio it guards. The WAL path never
/// routes through here. Chunk sizes on either leg are irrelevant: both are
/// re-framed internally, which is what makes the pump's uneven draining
/// survivable.
pub struct EchoGate {
    reference: EnvelopeTracker,
    mic: EnvelopeTracker,
    coupled_streak: u32,
    /// The best signed lag of the previous coupled-looking window, for the
    /// stability check.
    last_lag: Option<i64>,
    assessed: u64,
    suppressed: u64,
}

impl EchoGate {
    /// A gate for feeds at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let frame_len = FRAME_MS * sample_rate as usize / 1_000;
        let capacity = MAX_LAG_FRAMES + WINDOW_FRAMES * 2;
        debug_assert!(capacity >= MAX_LAG_FRAMES + WINDOW_FRAMES);
        Self {
            reference: EnvelopeTracker::new(frame_len, capacity),
            mic: EnvelopeTracker::new(frame_len, capacity),
            coupled_streak: 0,
            last_lag: None,
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

    /// Feed system-leg audio into the reference envelope.
    pub fn push_system(&mut self, samples: &[f32]) {
        self.reference.push(samples);
    }

    /// Score this mic chunk in context: the chunk joins the mic envelope
    /// history, and the score is the peak mean-removed correlation between
    /// the recent mic envelope and the reference envelope over the lag
    /// search. `&mut` because context is the point — the envelope history is
    /// what survives a room, a single chunk is not.
    pub fn score(&mut self, mic: &[f32]) -> f32 {
        self.scored(mic).best
    }

    /// [`EchoGate::score`], also reporting where the peak was and how the
    /// previous round's lag is scoring now.
    fn scored(&mut self, mic: &[f32]) -> Scored {
        self.mic.push(mic);

        let window = WINDOW_FRAMES.min(self.mic.frames.len());
        if window < WARMUP_FRAMES {
            return Scored::default();
        }
        let mic_diff = onset_pattern(&self.mic.frames[self.mic.frames.len() - window..]);
        if rms_of_window(&self.mic.frames[self.mic.frames.len() - window..]) < ACTIVITY_FLOOR {
            return Scored::default();
        }

        let ref_frames = &self.reference.frames;
        if ref_frames.len() < window {
            return Scored::default();
        }

        let mut out = Scored::default();
        let consider = |c: f32, lag: i64, out: &mut Scored, last: Option<i64>| {
            if c > out.best {
                out.best = c;
                out.best_lag = Some(lag);
            }
            // How yesterday's delay is doing today. The peak's exact
            // position jitters between near-equal neighbours; the question
            // that identifies a room is whether the OLD lag still scores,
            // not whether the argmax held still.
            if let Some(prev) = last
                && prev.abs_diff(lag) <= LAG_JITTER_FRAMES as u64
                && c > out.at_previous
            {
                out.at_previous = c;
            }
        };

        // Positive lags: mic-now against reference history.
        let max_pos = MAX_LAG_FRAMES.min(ref_frames.len() - window);
        for lag in 0..=max_pos {
            let end = ref_frames.len() - lag;
            let reference = &ref_frames[end - window..end];
            if rms_of_window(reference) < ACTIVITY_FLOOR {
                continue;
            }
            let c = pearson(&mic_diff, &onset_pattern(reference));
            consider(c, lag as i64, &mut out, self.last_lag);
        }

        // Negative lags: reference-now against mic history — the live
        // failure's direction, where the reference feed runs behind the mic
        // and the match has not "arrived" in reference history yet.
        let ref_tail = &ref_frames[ref_frames.len() - window..];
        if rms_of_window(ref_tail) >= ACTIVITY_FLOOR {
            let ref_diff = onset_pattern(ref_tail);
            let max_neg = MAX_LAG_FRAMES.min(self.mic.frames.len() - window);
            for back in 1..=max_neg {
                let end = self.mic.frames.len() - back;
                let mic_window = &self.mic.frames[end - window..end];
                if rms_of_window(mic_window) < ACTIVITY_FLOOR {
                    continue;
                }
                let c = pearson(&onset_pattern(mic_window), &ref_diff);
                consider(c, -(back as i64), &mut out, self.last_lag);
            }
        }
        out
    }

    /// Judge one mic chunk. Call once per chunk, after the same iteration's
    /// system audio was pushed.
    pub fn assess(&mut self, mic: &[f32]) -> GateVerdict {
        self.assessed += 1;
        let scored = self.scored(mic);

        // Coupled means the pattern matches; stable means the old delay is
        // still essentially THE peak — not merely acceptable. Rooms hold
        // still; coincidences teleport.
        let stable = scored.at_previous >= CORRELATION_THRESHOLD
            && scored.at_previous >= scored.best * STABILITY_RATIO;
        let coupled = scored.best >= CORRELATION_THRESHOLD;
        self.last_lag = scored.best_lag;

        if coupled && (stable || self.coupled_streak == 0) {
            self.coupled_streak = self.coupled_streak.saturating_add(1);
        } else {
            // Halve rather than reset: real echo dips under the threshold
            // on quiet syllables, and a hard reset made suppression strobe
            // through sustained playback — but a long streak must not take a
            // long time to release once headphones go on. Halving releases
            // any streak within two clean chunks, which the release test
            // pins, while a single mid-echo dip only dents the streak.
            self.coupled_streak /= 2;
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
}

/// Mean-removed normalized correlation between two equal-length windows.
///
/// Mean removal is what makes log-domain attenuation invisible: a quieter
/// copy of the same pattern scores as the same pattern.
fn pearson(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f32;
    let mean_a: f32 = a.iter().sum::<f32>() / n;
    let mean_b: f32 = b.iter().sum::<f32>() / n;
    let (mut dot, mut sq_a, mut sq_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        let (da, db) = (x - mean_a, y - mean_b);
        dot += da * db;
        sq_a += da * da;
        sq_b += db * db;
    }
    let denom = (sq_a * sq_b).sqrt();
    if denom <= f32::EPSILON {
        return 0.0;
    }
    // Only a positive match is a copy: an anti-correlated envelope is not an
    // echo, it is a coincidence of opposite rhythms.
    (dot / denom).max(0.0)
}

/// The syllable pattern: log energy with its own local trend removed.
///
/// A band-pass, in effect. Frame-to-frame differencing was tried first and
/// amplified exactly the wrong thing — the carrier's per-frame statistical
/// jitter — while the identity of speech lives in the 3–8 Hz syllable band.
/// Subtracting a short moving average keeps that band: slow trends (overall
/// loudness, the thing that makes unrelated speakers look alike) vanish,
/// syllable onsets survive, and frame jitter averages itself away inside
/// the window.
fn onset_pattern(log_frames: &[f32]) -> Vec<f32> {
    const TREND: usize = 15; // 150 ms: below the syllable band's period.
    let n = log_frames.len();
    (0..n)
        .map(|i| {
            let from = i.saturating_sub(TREND / 2);
            let to = (i + TREND / 2 + 1).min(n);
            let local: f32 = log_frames[from..to].iter().sum::<f32>() / (to - from) as f32;
            log_frames[i] - local
        })
        .collect()
}

/// Linear RMS of a log-energy window.
fn rms_of_window(log_frames: &[f32]) -> f32 {
    if log_frames.is_empty() {
        return 0.0;
    }
    let mean_sq: f32 = log_frames
        .iter()
        .map(|log| (log.exp() - LOG_FLOOR).max(0.0))
        .sum::<f32>()
        / log_frames.len() as f32;
    mean_sq.sqrt()
}

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
const CORRELATION_THRESHOLD: f32 = 0.55;

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

/// Maximum envelope lag searched, in frames (600 ms). Covers the acoustic
/// path, device latency, and — the part the first version missed — the
/// pump's skew between when each leg's audio reaches this gate.
const MAX_LAG_FRAMES: usize = 60;

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

/// One scoring pass over the lag search.
#[derive(Debug, Default)]
struct Scored {
    /// The peak correlation anywhere in the search.
    best: f32,
    /// Where the peak was.
    best_lag: Option<usize>,
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
    /// The best lag of the previous coupled-looking window, for the
    /// stability check.
    last_lag: Option<usize>,
    assessed: u64,
    suppressed: u64,
}

impl EchoGate {
    /// A gate for feeds at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let frame_len = FRAME_MS * sample_rate as usize / 1_000;
        let capacity = MAX_LAG_FRAMES + WINDOW_FRAMES * 2;
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

        let max_lag = MAX_LAG_FRAMES.min(ref_frames.len() - window);
        let mut out = Scored::default();
        for lag in 0..=max_lag {
            let end = ref_frames.len() - lag;
            let reference = &ref_frames[end - window..end];
            if rms_of_window(reference) < ACTIVITY_FLOOR {
                continue;
            }
            let c = pearson(&mic_diff, &onset_pattern(reference));
            if c > out.best {
                out.best = c;
                out.best_lag = Some(lag);
            }
            // How yesterday's delay is doing today. The peak's exact position
            // jitters between near-equal neighbours; the question that
            // identifies a room is whether the OLD lag still scores, not
            // whether the argmax held still.
            if let Some(prev) = self.last_lag
                && prev.abs_diff(lag) <= LAG_JITTER_FRAMES
                && c > out.at_previous
            {
                out.at_previous = c;
            }
        }
        out
    }

    /// Judge one mic chunk. Call once per chunk, after the same iteration's
    /// system audio was pushed.
    pub fn assess(&mut self, mic: &[f32]) -> GateVerdict {
        self.assessed += 1;
        let scored = self.scored(mic);

        // Coupled means the pattern matches; stable means the delay it
        // matched at last time still matches now. Rooms hold still, so real
        // echo keeps scoring at its old lag even while the argmax jitters
        // between near-equal neighbours; a coincidence between independent
        // speakers scores somewhere new each time.
        let stable = scored.at_previous >= CORRELATION_THRESHOLD;
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

/// The frame-to-frame change in log energy: the onset train.
///
/// Differencing whitens the slow loudness curve that makes two unrelated
/// speakers look alike over a short window — what remains is *when* energy
/// arrives, which is the fingerprint a room cannot fake and a copy cannot
/// hide.
fn onset_pattern(log_frames: &[f32]) -> Vec<f32> {
    log_frames.windows(2).map(|w| w[1] - w[0]).collect()
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

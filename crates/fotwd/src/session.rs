//! The meeting session: everything from a microphone to a stored transcript.
//!
//! This is the composition root. Each crate below it is independently
//! testable; this module is where they become a meeting recorder.
//!
//! ```text
//!  system tap ─┐                             ┌─→ WAL system.pcm  (raw, crash-safe)
//!              ├─→ ring ─→ pump ─→ resample ─┤
//!  mic tap ────┘   (RT)   (normal)  48k→16k  └─→ STT ─→ segments ─┬→ WAL stt.jsonl
//!                                              mono i16           └→ SQLite
//! ```
//!
//! # Two invariants this layering exists to protect
//!
//! **Raw audio is written before anything derived.** The WAL write happens on
//! the pump thread whether or not a provider is configured, whether or not
//! the network is up, and whether or not transcription succeeds. A meeting is
//! never lost because an API key expired.
//!
//! **The network never reaches the audio thread.** The real-time callback
//! copies into a ring and returns. Everything that can block — disk, TLS,
//! sockets — happens downstream of a bounded queue, and a stalled provider
//! degrades the transcript rather than the recording.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use fotw_audio::{AudioTap, CaptureTimestamp, FrameFlags, FrameSink, StreamFormat, TapError};
use fotw_pipeline::resample::{Downmixer, Resampler16k};
use fotw_pipeline::ring::{AudioRing, RingConsumer, RingProducer};
use fotw_pipeline::wal::{SessionWal, SttRecord};
use fotw_stt::deepgram::DeepgramConfig;
use fotw_stt::{DeepgramStream, DeepgramStreamConfig, Source, StreamEvent, TranscriptSegment};

/// Ring capacity per leg, in samples. Ten seconds at 48 kHz stereo.
const RING_SAMPLES: usize = 48_000 * 2 * 10;

/// How long the pump waits when both rings are empty.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// What a session produced.
#[derive(Debug, Default)]
pub struct SessionOutcome {
    /// Where the session lives on disk.
    pub dir: PathBuf,
    /// Wall clock when capture actually began, in epoch milliseconds.
    ///
    /// Carried explicitly rather than stamped at persist time: those differ by
    /// the length of the meeting, and using the later one makes every
    /// recording show a duration of zero.
    pub started_at_ms: u64,
    /// Interleaved samples written for the system leg.
    pub system_samples: u64,
    /// Interleaved samples written for the mic leg.
    pub mic_samples: u64,
    /// How this meeting was degraded, if it was: what the transcription
    /// provider failed with, and what capture lost on the way in.
    ///
    /// Empty is not the same as "transcription worked": a session with
    /// [`Transcription::Disabled`] also has none. The two are distinguished by
    /// what the caller configured, not by this field.
    ///
    /// Named for the provider because that is all it used to carry. Ring drops
    /// joined it in #79 rather than getting a channel of their own: this is the
    /// only degradation channel with a reader on the other end.
    pub stt_errors: Vec<String>,
    /// Buffers the tap delivered that were digitally silent.
    pub silent_buffers: u64,
    /// Total buffers the tap delivered.
    pub total_buffers: u64,
    /// Samples the ring dropped because the pump fell behind.
    pub dropped_samples: u64,
    /// Finalized transcript segments.
    pub segments: Vec<TranscriptSegment>,
}

impl SessionOutcome {
    /// Whether any audible audio was captured at all.
    #[must_use]
    pub const fn captured_audio(&self) -> bool {
        self.total_buffers > 0 && self.silent_buffers < self.total_buffers
    }

    /// The transcript as plain text.
    #[must_use]
    pub fn transcript_text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A sink that copies into a ring and returns. Nothing else may happen here.
struct RingSink {
    producer: RingProducer,
    silent: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

impl FrameSink for RingSink {
    fn on_frames(&mut self, pcm: &[f32], _ts: CaptureTimestamp, flags: FrameFlags) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if flags.contains(FrameFlags::SILENT) {
            self.silent.fetch_add(1, Ordering::Relaxed);
        }
        // The return value is deliberately ignored: a short write means the
        // pump is behind, and retrying on a real-time thread is blocking by
        // another name. The shortfall is counted for the pump to surface.
        let _ = self.producer.push_block(pcm);
    }

    fn on_error(&mut self, _e: TapError) {}
}

/// One Deepgram connection per capture leg.
///
/// Spec 7.5 called two cloud streams an explicit decision because the second
/// one doubles the bill. The decision, made in issue #60: the mic leg is **on
/// by default** — a meeting-notes tool that omits the note-taker's half of
/// every conversation fails at its one job — and declined explicitly with
/// `FOTW_MIC_STT=off`, because a bill must be declinable.
pub struct DeepgramLegs {
    /// The far end: everybody else on the call. Always transcribed.
    pub system: Box<DeepgramStreamConfig>,
    /// The near end: the user. `None` on a machine with no input device, or
    /// when the opt-out is set. The session also refuses to open this stream
    /// if the mic tap did not actually start — a paid connection fed nothing
    /// would be pure cost.
    pub mic: Option<Box<DeepgramStreamConfig>>,
}

impl DeepgramLegs {
    /// The legs for one session, from the one key both connections share.
    ///
    /// Diarization asymmetry is inherited from [`DeepgramConfig::new`]: the
    /// system leg is diarized (`S0`, `S1`, …), the mic leg is not — it is one
    /// known person, and the normalizer labels it `me`.
    #[must_use]
    pub fn for_session(
        api_key: &str,
        session_id: &str,
        mic_present: bool,
        mic_enabled: bool,
    ) -> Self {
        let system = Box::new(DeepgramStreamConfig::new(
            api_key.to_owned(),
            DeepgramConfig::new(session_id.to_owned(), Source::System),
        ));
        let mic = (mic_present && mic_enabled).then(|| {
            Box::new(DeepgramStreamConfig::new(
                api_key.to_owned(),
                DeepgramConfig::new(session_id.to_owned(), Source::Mic),
            ))
        });
        Self { system, mic }
    }
}

/// Whether the mic leg should be transcribed, from `FOTW_MIC_STT`.
///
/// Unset means on. Only the documented `off` and the spellings muscle memory
/// produces (`0`, `false`, any casing) mean off — a typo must fail toward the
/// documented default, and `FOTW_MIC_STT=on` set by a script must not read as
/// an opt-out.
#[must_use]
pub fn mic_stt_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false")
    })
}

/// Whether the speaker-echo gate guards the mic's transcription feed.
///
/// From `FOTW_ECHO_GATE`; unset means on. Off is for A/B against the gate —
/// CAP-11's acceptance metric needs a control group — and the same spellings
/// as `FOTW_MIC_STT` mean off, for the same muscle-memory reasons.
#[must_use]
pub fn echo_gate_enabled(value: Option<&str>) -> bool {
    mic_stt_enabled(value)
}

/// Whether the cross-leg transcript dedupe runs at persist time.
///
/// From `FOTW_TEXT_DEDUPE`; unset means on. Off is the field escape hatch,
/// and the A/B control that keeps the audio gate's CAP-11 acceptance metric
/// measurable — unconditional text dedupe would erase transcript-level
/// duplicates in both arms of that experiment.
#[must_use]
pub fn text_dedupe_enabled(value: Option<&str>) -> bool {
    mic_stt_enabled(value)
}

/// Cross-leg transcript dedupe: drop mic-leg segments that duplicate the
/// system leg. Returns how many were dropped.
///
/// On speakers the mic re-transcribes the system audio, so the same passage
/// lands twice — once diarized on the system leg, once labeled `me` — with
/// ASR wording drift between the copies and multi-second skew between the
/// legs. The audio gate (CAP-11 v1) removes what it can before transcription;
/// this pass removes what leaks, because in the text domain the duplication
/// is trivially visible no matter what the room did to the waveform.
/// Precedent: Descript's "mic bleed" fix — text-only removal, audio
/// untouched. The system copy always wins: it is the clean, diarized feed.
///
/// # How a mic segment is judged
///
/// Tokens come from [`fotw_stt::normalize_tokens`], then — here only, never
/// in `normalize_tokens` itself, whose one-unit-one-token contract STT-09's
/// replay trimming depends on — digits and number words collapse to one
/// class token, because the two legs routinely disagree on "29" versus
/// "twenty nine" while transcribing the same audio.
///
/// * **Containment** (four tokens or more): the segment's tokens against the
///   concatenated multiset of every system segment overlapping its span
///   padded by [`DEDUPE_WINDOW_MS`]. Concatenated, not element-wise max — a
///   phrase played twice was transcribed twice and may match twice.
/// * **Short fragments** (under four tokens — the audio gate's warmup
///   residue): a contiguous-subsequence match inside a *single* system
///   segment overlapping within [`DEDUPE_SHORT_WINDOW_MS`]. Tight on
///   purpose: an echo overlaps its source, a spoken confirmation follows it.
///
/// # The corroboration guard
///
/// Echo is never a one-off: a session where exactly one mic utterance
/// matches moderately is a human repeating something, not a room. A
/// candidate is dropped only if it is near-verbatim on its own
/// ([`DEDUPE_VERBATIM`]), or clears [`DEDUPE_THRESHOLD`] while at least one
/// *other* mic segment in the session also clears it. Short fragments always
/// need that corroboration — a two-word interjection can be a contiguous
/// coincidence, and eating the user's real voice is the one unforgivable
/// failure here exactly as it is in the audio gate.
pub fn dedupe_cross_leg(segments: &mut Vec<TranscriptSegment>) -> usize {
    let system: Vec<(u64, u64, Vec<String>)> = segments
        .iter()
        .filter(|s| s.source == Source::System)
        .map(|s| (s.start_ms, s.end_ms, classed_tokens(&s.text)))
        .collect();
    if system.is_empty() {
        return 0;
    }

    /// What pass one learned about one mic segment.
    struct Candidate {
        index: usize,
        containment: f32,
        short: bool,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut clearers = 0usize;
    for (index, seg) in segments.iter().enumerate() {
        if seg.source != Source::Mic {
            continue;
        }
        let tokens = classed_tokens(&seg.text);
        if tokens.is_empty() {
            continue;
        }

        if tokens.len() >= DEDUPE_MIN_MATCHED {
            let from = seg.start_ms.saturating_sub(DEDUPE_WINDOW_MS);
            let to = seg.end_ms + DEDUPE_WINDOW_MS;
            let mut window: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for (start, end, sys_tokens) in &system {
                if *start <= to && *end >= from {
                    for t in sys_tokens {
                        *window.entry(t.as_str()).or_insert(0) += 1;
                    }
                }
            }
            let mut counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for t in &tokens {
                *counts.entry(t.as_str()).or_insert(0) += 1;
            }
            let matched: usize = counts
                .iter()
                .map(|(t, n)| (*n).min(window.get(t).copied().unwrap_or(0)))
                .sum();
            let containment = matched as f32 / tokens.len() as f32;
            if containment >= DEDUPE_THRESHOLD && matched >= DEDUPE_MIN_MATCHED {
                clearers += 1;
                candidates.push(Candidate {
                    index,
                    containment,
                    short: false,
                });
            }
        } else {
            let from = seg.start_ms.saturating_sub(DEDUPE_SHORT_WINDOW_MS);
            let to = seg.end_ms + DEDUPE_SHORT_WINDOW_MS;
            let echoed = system.iter().any(|(start, end, sys_tokens)| {
                *start <= to
                    && *end >= from
                    && sys_tokens
                        .windows(tokens.len())
                        .any(|w| w == tokens.as_slice())
            });
            if echoed {
                candidates.push(Candidate {
                    index,
                    containment: 1.0,
                    short: true,
                });
            }
        }
    }

    let mut drop: Vec<usize> = Vec::new();
    for candidate in &candidates {
        let corroborated = clearers >= if candidate.short { 1 } else { 2 };
        let alone_is_enough = !candidate.short && candidate.containment >= DEDUPE_VERBATIM;
        if alone_is_enough || corroborated {
            drop.push(candidate.index);
        }
    }
    let dropped = drop.len();
    let mut keep_index = 0usize;
    segments.retain(|_| {
        let keep = !drop.contains(&keep_index);
        keep_index += 1;
        keep
    });
    dropped
}

/// Containment above which a long mic segment reads as an echo copy.
///
/// Derived from live pairs in `tests/cross_leg_dedupe.rs` — real podcast
/// echo lands 0.83–1.0 with number-classing, independent speech under 0.4.
const DEDUPE_THRESHOLD: f32 = 0.65;

/// Containment at which a lone long match is echo even without
/// corroboration: near-verbatim duplication does not happen by accident.
const DEDUPE_VERBATIM: f32 = 0.85;

/// Minimum matched tokens for the containment path, and the boundary below
/// which the short-fragment rule applies instead. Four: any three common
/// words appear somewhere in twenty seconds of speech.
const DEDUPE_MIN_MATCHED: usize = 4;

/// Span padding for the containment window, covering the observed
/// multi-second skew between the legs' clocks.
const DEDUPE_WINDOW_MS: u64 = 10_000;

/// Span padding for the short-fragment rule — tight, bounded by the audio
/// gate's own search horizon: an echo overlaps its source, a spoken
/// confirmation follows it.
const DEDUPE_SHORT_WINDOW_MS: u64 = 3_500;

/// Whether `text` reads as an echo of any of the recent system token sets —
/// the live half of the cross-leg dedupe, sharing the persist pass's rules:
/// containment for four-plus tokens, contiguous subsequence for fragments.
/// No corroboration guard here — the live view is cosmetic and corrected at
/// persist either way, and a guard would need session history a streaming
/// callback does not hold.
pub fn echoes_recent<'a>(text: &str, recent_system: impl Iterator<Item = &'a [String]>) -> bool {
    let tokens = classed_tokens(text);
    if tokens.is_empty() {
        return false;
    }
    let recent: Vec<&[String]> = recent_system.collect();
    if recent.is_empty() {
        return false;
    }
    if tokens.len() >= DEDUPE_MIN_MATCHED {
        let mut window: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for set in &recent {
            for t in set.iter() {
                *window.entry(t.as_str()).or_insert(0) += 1;
            }
        }
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for t in &tokens {
            *counts.entry(t.as_str()).or_insert(0) += 1;
        }
        let matched: usize = counts
            .iter()
            .map(|(t, n)| (*n).min(window.get(t).copied().unwrap_or(0)))
            .sum();
        matched >= DEDUPE_MIN_MATCHED && matched as f32 / tokens.len() as f32 >= DEDUPE_THRESHOLD
    } else {
        recent
            .iter()
            .any(|set| set.windows(tokens.len()).any(|w| w == tokens.as_slice()))
    }
}

/// The dedupe's token view of a text, for callers holding their own window.
#[must_use]
pub fn dedupe_tokens(text: &str) -> Vec<String> {
    classed_tokens(text)
}

/// [`fotw_stt::normalize_tokens`], with digits and number words collapsed to
/// one class token. Local to the dedupe on purpose — see the fn doc.
fn classed_tokens(text: &str) -> Vec<String> {
    const NUMBER_WORDS: &[&str] = &[
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
        "thirty",
        "forty",
        "fifty",
        "sixty",
        "seventy",
        "eighty",
        "ninety",
        "hundred",
        "thousand",
        "million",
        "billion",
        "oh",
        "o",
    ];
    fotw_stt::normalize_tokens(text)
        .into_iter()
        .map(|t| {
            if t.chars().all(|c| c.is_ascii_digit()) || NUMBER_WORDS.contains(&t.as_str()) {
                "<num>".to_owned()
            } else {
                t
            }
        })
        .collect()
}

/// Put segments from both legs into spoken order.
///
/// Two streams finalize independently, so segments arrive interleaved by
/// network luck rather than by when the words were said; unmerged, a
/// two-person exchange reads as two monologues. Stable, so equal start times
/// keep arrival order and the same meeting always renders byte-identically.
pub fn order_segments(segments: &mut [TranscriptSegment]) {
    segments.sort_by_key(|s| s.start_ms);
}

/// How to transcribe, if at all.
pub enum Transcription {
    /// Record only. Still fully useful — the audio can be transcribed later.
    Disabled,
    /// Stream to Deepgram as the meeting runs, one connection per leg.
    Deepgram(DeepgramLegs),
}

impl std::fmt::Debug for Transcription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "Disabled"),
            Self::Deepgram(_) => write!(f, "Deepgram(..)"),
        }
    }
}

/// A latch an external caller can use to end a running session.
///
/// The CLI never needs one — `fotwd record 3600` ends on its stopwatch — but a
/// Start/Stop button does: nobody knows how long a meeting is when it begins.
///
/// # Why a `Notify` and not a bare `AtomicBool`
///
/// The pump already polls its own latch every [`IDLE_POLL`], because it is a
/// blocking thread with nowhere to await. The async side has somewhere to
/// await, and a second poller would be a wake-up ten times a second for a flag
/// that changes exactly once. `Notify` lets the session sleep until either the
/// duration expires or someone asks it to stop.
#[derive(Clone, Debug, Default)]
pub struct StopSignal(Arc<StopInner>);

#[derive(Debug, Default)]
struct StopInner {
    requested: AtomicBool,
    notify: tokio::sync::Notify,
}

impl StopSignal {
    /// A signal nobody has tripped yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the session to end. Idempotent; a second call is a no-op.
    pub fn stop(&self) {
        self.0.requested.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    /// Whether [`stop`](Self::stop) has been called.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.0.requested.load(Ordering::Acquire)
    }

    /// Resolve once the signal is tripped, now or later.
    ///
    /// The `notified()` future is constructed *before* the flag is read, and
    /// that order is the whole correctness argument: `notify_waiters` wakes
    /// only those already waiting and leaves no permit behind, so a `stop()`
    /// landing between a read and an await would be lost forever. Registering
    /// first turns that race into a redundant wake-up.
    async fn wait(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.is_stopped() {
                return;
            }
            notified.await;
        }
    }
}

/// A latch the session trips once capture is genuinely live.
///
/// The counterpart to [`StopSignal`], and the answer to a specific bug: the
/// taps are started *inside* the session, so a caller that spawned it and
/// returned would report "recording" while a Core Audio device sat blocked in
/// `start()`. That produced a red badge, a ticking clock and an empty disk —
/// the one failure mode this project exists to avoid.
///
/// # Why a `Condvar` and not a `Notify`
///
/// The waiter is not async. `RecorderControl::start` is called on a blocking
/// pool thread and has to answer before it returns, so it needs a blocking
/// wait with a deadline; the signaller is on the runtime and only sets a flag.
#[derive(Clone, Debug, Default)]
pub struct ReadySignal(Arc<ReadyInner>);

#[derive(Debug, Default)]
struct ReadyInner {
    ready: std::sync::Mutex<bool>,
    woke: std::sync::Condvar,
}

impl ReadySignal {
    /// A signal nobody has tripped yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Announce that capture is live. Idempotent.
    pub fn signal(&self) {
        let mut ready = self.0.ready.lock().unwrap_or_else(|e| e.into_inner());
        *ready = true;
        // `notify_all`, not `notify_one`: the flag is level-triggered and more
        // than one thread may be waiting on a slow start.
        self.0.woke.notify_all();
    }

    /// Whether capture has been announced.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        *self.0.ready.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until capture is live, or `timeout` elapses.
    ///
    /// Returns whether it became ready. Loops on the predicate rather than
    /// trusting a single wake, because a `Condvar` may wake spuriously.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut ready = self.0.ready.lock().unwrap_or_else(|e| e.into_inner());
        while !*ready {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            let (guard, _) = self
                .0
                .woke
                .wait_timeout(ready, left)
                .unwrap_or_else(|e| e.into_inner());
            ready = guard;
        }
        true
    }
}

/// What the transcription provider said went wrong, as it happens.
///
/// # Why this type exists at all
///
/// `run` used to consume `StreamEvent::Final` and drop `StreamEvent::Error`.
/// Two separate bugs each killed the Deepgram stream on connect — a handshake
/// the provider rejected, and a frame shape the reader could not parse — and
/// neither produced a single line of output anywhere. An empty `stt.jsonl`
/// beside hours of good audio is indistinguishable from a meeting where nobody
/// spoke, so both survived until someone went looking with a packet-level
/// probe. A failure nobody can see is a failure nobody fixes.
#[derive(Clone, Debug, Default)]
pub struct SttErrors(Arc<std::sync::Mutex<Vec<String>>>);

impl SttErrors {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that the provider failed.
    pub fn record(&self, message: impl Into<String>) {
        self.lock().push(message.into());
    }

    /// The most recent failure, if any.
    ///
    /// The latest rather than the first: a stream that reconnects and fails
    /// again should surface why it is failing *now*, not the reason it first
    /// tripped an hour ago.
    #[must_use]
    pub fn latest(&self) -> Option<String> {
        self.lock().last().cloned()
    }

    /// How many failures have been seen.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// Take everything recorded so far.
    #[must_use]
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The closure shape a [`SegmentTap`] holds.
type SegmentFn = dyn Fn(&TranscriptSegment, TapKind) + Send + Sync;

/// A callback the collector hands every finalized segment, as it arrives.
///
/// This is the live-transcript producer seam (#61): the hub, its flusher and
/// the WebSocket all existed with nothing feeding them, so a transcript was
/// only ever visible after the meeting finalized. The default is silence —
/// every caller that does not name the feature keeps its old behavior.
///
/// A wrapper rather than a bare `Option<Arc<dyn Fn>>` so `SessionControl`
/// keeps deriving `Clone` and `Default`, and so `Debug` can redact: segments
/// are meeting content, and §10's never-log rule does not stop at log files.
#[derive(Clone, Default)]
pub struct SegmentTap(Option<Arc<SegmentFn>>);

/// Whether a tapped segment is settled or still being revised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapKind {
    /// A revision of an utterance in progress. The next partial with the
    /// same utterance replaces it; only the final is stored.
    Partial,
    /// Settled text. This is what the sink buffers and persist writes.
    Final,
}

impl std::fmt::Debug for SegmentTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "SegmentTap(<set>)"
        } else {
            "SegmentTap(<none>)"
        })
    }
}

impl SegmentTap {
    /// A tap that hands each segment to `f`.
    ///
    /// `f` runs on the collector task while the meeting is live, so it must
    /// not block: the hub's `publish` is a buffered push by design.
    #[must_use]
    pub fn new(f: impl Fn(&TranscriptSegment, TapKind) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Hand one segment over, or do nothing when no tap was set.
    pub fn emit(&self, segment: &TranscriptSegment, kind: TapKind) {
        if let Some(f) = &self.0 {
            f(segment, kind);
        }
    }
}

/// The two latches a caller outside the session holds.
#[derive(Clone, Debug, Default)]
pub struct SessionControl {
    /// Trip to end the session early.
    pub stop: StopSignal,
    /// Tripped by the session once capture is live.
    pub ready: ReadySignal,
    /// Filled in by the session when the transcription provider fails.
    pub errors: SttErrors,
    /// Handed every finalized segment as it arrives, for the live transcript.
    pub on_segment: SegmentTap,
}

impl SessionControl {
    /// A fresh pair.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Run a meeting session to completion.
///
/// `system` is required; `mic` is optional so a machine with no input device
/// still records the far end rather than refusing to start.
pub async fn run(
    root: &Path,
    system: Box<dyn AudioTap>,
    mic: Option<Box<dyn AudioTap>>,
    transcription: Transcription,
    duration: Duration,
) -> Result<SessionOutcome, String> {
    run_with_stop(
        root,
        system,
        mic,
        transcription,
        duration,
        StopSignal::new(),
    )
    .await
}

/// [`run`], with the stop latch named.
///
/// The seam that lets a caller who did not start a stopwatch still end the
/// session — mirroring `open_library` / `open_library_with` and
/// `AuditLog::record` / `record_at` elsewhere in this crate.
///
/// `duration` remains a ceiling rather than becoming optional. An unbounded
/// session is a real failure mode: it runs until the disk fills, and
/// [`crate::retention::recording_in_flight`] vetoes the sweeper for as long as
/// it is alive, so a forgotten recording disables retention too. Whichever
/// arrives first wins.
///
/// # Errors
///
/// Whatever [`run`] fails with.
pub async fn run_with_stop(
    root: &Path,
    system: Box<dyn AudioTap>,
    mic: Option<Box<dyn AudioTap>>,
    transcription: Transcription,
    duration: Duration,
    stop: StopSignal,
) -> Result<SessionOutcome, String> {
    run_with_control(
        root,
        system,
        mic,
        transcription,
        duration,
        SessionControl {
            stop,
            ready: ReadySignal::new(),
            errors: SttErrors::new(),
            on_segment: SegmentTap::default(),
        },
    )
    .await
}

/// [`run_with_stop`], and the caller also learns when capture went live.
///
/// # Errors
///
/// Whatever [`run`] fails with.
pub async fn run_with_control(
    root: &Path,
    mut system: Box<dyn AudioTap>,
    mut mic: Option<Box<dyn AudioTap>>,
    transcription: Transcription,
    duration: Duration,
    control: SessionControl,
) -> Result<SessionOutcome, String> {
    let stop_signal = control.stop;
    let silent = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));

    // Every allocation happens here, before anything real-time is running.
    let (sys_prod, sys_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
    let (mic_prod, mic_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);

    let sys_format = system
        .start(Box::new(RingSink {
            producer: sys_prod,
            silent: Arc::clone(&silent),
            total: Arc::clone(&total),
        }))
        .map_err(|e| format!("could not start the system tap: {e}"))?;

    let mic_format = mic.as_mut().and_then(|t| {
        t.start(Box::new(RingSink {
            producer: mic_prod,
            silent: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
        }))
        .ok()
    });

    let wal = SessionWal::create(root, sys_format.sample_rate_hz, sys_format.channels)
        .map_err(|e| format!("could not create the session: {e}"))?;
    let dir = wal.dir().to_path_buf();
    let started_at_ms = wal.manifest().started_at_ms;

    // The STT side, if configured. `write` is non-blocking, so the pump can
    // feed it without ever waiting on the network.
    // One stream per leg. The mic stream is additionally gated on the mic tap
    // having actually started: a paid connection for a device that is not
    // there would be fed nothing and still billed for the socket.
    let (sys_stt, sys_events, mic_stt, mic_events) = match transcription {
        Transcription::Disabled => (None, None, None, None),
        Transcription::Deepgram(legs) => {
            let (s, s_rx) = DeepgramStream::open(*legs.system);
            let (m, m_rx) = match legs.mic {
                Some(cfg) if mic_format.is_some() => {
                    let (m, rx) = DeepgramStream::open(*cfg);
                    (Some(Arc::new(m)), Some(rx))
                }
                _ => (None, None),
            };
            (Some(Arc::new(s)), Some(s_rx), m, m_rx)
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = Arc::clone(&stop);
    let feeds = SttFeeds {
        system: sys_stt.clone().map(|s| s as Arc<dyn PcmFeed>),
        mic: mic_stt.clone().map(|s| s as Arc<dyn PcmFeed>),
        echo_gate: (sys_stt.is_some()
            && mic_stt.is_some()
            && echo_gate_enabled(std::env::var("FOTW_ECHO_GATE").ok().as_deref()))
        .then(|| fotw_pipeline::echo::EchoGate::new(16_000)),
    };

    // The pump owns the WAL and does every blocking thing.
    let pump = std::thread::spawn(move || -> Result<(u64, u64, u64), String> {
        pump_loop(
            wal, sys_cons, mic_cons, sys_format, mic_format, feeds, &pump_stop,
        )
    });

    // Drain transcript events while the meeting runs, so a long meeting does
    // not accumulate an unbounded queue. One collector per leg, into one
    // shared sink — the merge is a sort at the end, not a select here.
    let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sys_collector = sys_events.map(|rx| {
        spawn_leg_collector(
            "system",
            rx,
            Arc::clone(&collected),
            control.errors.clone(),
            control.on_segment.clone(),
        )
    });
    let mic_collector = mic_events.map(|rx| {
        spawn_leg_collector(
            "mic",
            rx,
            Arc::clone(&collected),
            control.errors.clone(),
            control.on_segment.clone(),
        )
    });

    // Capture is genuinely live now: both taps returned from `start`, the WAL
    // exists and the pump is draining. Announced here rather than at the top
    // of the function because a tap that blocks in `start` never reaches this
    // line — which is the whole point.
    control.ready.signal();

    // Whichever comes first. `select!` drops the loser, and both branches are
    // cancel-safe: a dropped `sleep` is a dropped timer, and a dropped
    // `wait()` is a dropped `Notified` registration.
    tokio::select! {
        () = tokio::time::sleep(duration) => {}
        () = stop_signal.wait() => {}
    }

    // Stop capture first, then let the pump drain what is already buffered.
    let _ = system.stop();
    if let Some(t) = mic.as_mut() {
        let _ = t.stop();
    }
    stop.store(true, Ordering::Release);
    let (system_samples, mic_samples, dropped_samples) =
        pump.join().map_err(|_| "pump panicked".to_string())??;

    // Audio the pump was too slow to collect is audio that exists nowhere: the
    // ring sits upstream of the WAL, so a drop is not a degraded transcript,
    // it is a hole in the recording. It rides the same channel as a provider
    // failure because that is the only one a human ever sees — the field below
    // has never been read by anything (#79).
    if dropped_samples > 0 {
        control.errors.record(format!(
            "capture: {dropped_samples} samples were dropped at a full ring — \
             the pump could not keep up with the audio thread"
        ));
    }

    for stream in [sys_stt, mic_stt].into_iter().flatten() {
        let _ = stream.flush().await;
        let _ = stream.close().await;
    }
    for collector in [sys_collector, mic_collector].into_iter().flatten() {
        // The stream is closed, so the channel ends and this finishes.
        let _ = tokio::time::timeout(Duration::from_secs(10), collector).await;
    }

    let mut segments = collected
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .split_off(0);
    // Both legs finalized independently; put the conversation back in spoken
    // order before anything downstream renders or stores it.
    order_segments(&mut segments);

    Ok(SessionOutcome {
        dir,
        started_at_ms,
        system_samples,
        mic_samples,
        silent_buffers: silent.load(Ordering::Relaxed),
        total_buffers: total.load(Ordering::Relaxed),
        dropped_samples,
        segments,
        // Drained after the collector has finished, so a failure that arrived
        // during the provider's final flush is still carried out.
        stt_errors: control.errors.drain(),
    })
}

/// Drain one leg's transcript events into the shared sink.
///
/// The error arm is named per leg — a dead mic stream must not read as a dead
/// system stream, or the user turns the wrong knob. Recording rather than
/// dropping these is the line whose absence hid two fatal bugs for the life
/// of the project.
fn spawn_leg_collector(
    leg: &'static str,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    sink: Arc<std::sync::Mutex<Vec<TranscriptSegment>>>,
    errors: SttErrors,
    tap: SegmentTap,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Final(seg) => {
                    // Live first, then the buffer: a viewer watching the
                    // meeting should not be behind the file on disk.
                    tap.emit(&seg, TapKind::Final);
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(seg);
                }
                // Partials feed the live view and nothing else: the next
                // revision replaces them, only finals are stored, and
                // dropping them here is why the "live" transcript only
                // moved at utterance boundaries — one to three seconds
                // after the speaker paused, which reads as "not realtime".
                StreamEvent::Partial(seg) => tap.emit(&seg, TapKind::Partial),
                StreamEvent::Error(e) => {
                    eprintln!("  ! transcription ({leg}): {e}");
                    errors.record(format!("{leg}: {e}"));
                }
                _ => {}
            }
        }
    })
}

/// One leg's provider connection, as narrow as the pump's use of it.
///
/// The pump only ever hands a leg 16 kHz mono PCM, and how much of it a leg
/// has been handed *is that leg's clock* — Deepgram stamps a segment by how
/// much audio the socket has swallowed, nothing else. A trait here is what
/// makes that count observable without a socket, which is what issue #79's
/// cross-leg tests need.
trait PcmFeed: Send + Sync {
    /// Hand the provider 16-bit little-endian mono PCM.
    fn write_pcm(&self, pcm: &[i16]);
}

impl PcmFeed for DeepgramStream {
    fn write_pcm(&self, pcm: &[i16]) {
        self.write(pcm);
    }
}

/// The provider connections the pump feeds, one per leg.
///
/// A struct rather than two more parameters: the pump's argument list was at
/// clippy's limit, and these two travel together or not at all.
struct SttFeeds {
    system: Option<Arc<dyn PcmFeed>>,
    mic: Option<Arc<dyn PcmFeed>>,
    /// CAP-11 v1 (#71): withholds echo-dominated mic chunks from the mic
    /// feed when the mic is judged to be hearing the speakers. Present only
    /// when both feeds are live — with one leg there is nothing to couple.
    echo_gate: Option<fotw_pipeline::echo::EchoGate>,
}

/// Drain both rings until stopped, writing raw audio and feeding the provider.
fn pump_loop(
    mut wal: SessionWal,
    mut sys: RingConsumer,
    mut mic: RingConsumer,
    sys_format: StreamFormat,
    mic_format: Option<StreamFormat>,
    mut stt: SttFeeds,
    stop: &AtomicBool,
) -> Result<(u64, u64, u64), String> {
    let mut scratch = vec![0.0f32; 48_000];
    let (mut sys_written, mut mic_written) = (0u64, 0u64);
    // What a suppressed mic chunk is fed as. Kept across iterations because
    // chunk sizes settle: after the first few rounds this stops growing.
    let mut hush: Vec<i16> = Vec::new();

    let mut resampler = Resampler16k::new(sys_format.sample_rate_hz, sys_format.channels)
        .map_err(|e| format!("resampler: {e}"))?;

    // The mic gets its own resampler because its format is its own: the
    // system tap is 48k stereo, a USB headset mic is whatever it is, and
    // reusing the system resampler was never an option — which is exactly why
    // the old single-stream code had a `let _ = mic_format;` here.
    let mut mic_resampler = match (&stt.mic, mic_format) {
        (Some(_), Some(f)) => Some((
            Resampler16k::new(f.sample_rate_hz, f.channels)
                .map_err(|e| format!("mic resampler: {e}"))?,
            f.channels,
        )),
        _ => None,
    };

    let mut seq = 0u64;

    loop {
        let mut moved = false;

        let n = sys.pop_into(&mut scratch);
        if n > 0 {
            // Raw first, always. Derived work can fail; this must not.
            wal.write_system(&scratch[..n])
                .map_err(|e| format!("system write failed: {e}"))?;
            sys_written += n as u64;
            moved = true;

            if let Some(h) = stt.system.as_ref() {
                let resampled = resampler
                    .process_all(&scratch[..n])
                    .map_err(|e| format!("resample failed: {e}"))?;
                if !resampled.is_empty() {
                    let mono = Downmixer::to_mono(&resampled, sys_format.channels);
                    // The gate's reference: what the speakers are playing is
                    // exactly what the system feed carries.
                    if let Some(gate) = stt.echo_gate.as_mut() {
                        gate.push_system(&mono);
                    }
                    h.write_pcm(&Downmixer::to_i16(&mono));
                }
            }
        }

        let m = mic.pop_into(&mut scratch);
        if m > 0 {
            // Raw first here too, for the same reason as the system leg.
            wal.write_mic(&scratch[..m])
                .map_err(|e| format!("mic write failed: {e}"))?;
            mic_written += m as u64;
            moved = true;

            if let (Some(h), Some((r, channels))) = (stt.mic.as_ref(), mic_resampler.as_mut()) {
                let resampled = r
                    .process_all(&scratch[..m])
                    .map_err(|e| format!("mic resample failed: {e}"))?;
                if !resampled.is_empty() {
                    let mono = Downmixer::to_mono(&resampled, *channels);
                    // CAP-11 v1: on speakers the mic is mostly a copy of the
                    // system leg — transcribing it again costs money and
                    // attributes the far end's words to the user. Withheld
                    // from the FEED only; the WAL above already has the raw
                    // audio, so this is revertible in principle (the same
                    // text-not-audio stance Descript documents for its
                    // mic-bleed fix).
                    let suppress = stt.echo_gate.as_mut().is_some_and(|g| {
                        g.assess(&mono) == fotw_pipeline::echo::GateVerdict::Suppress
                    });
                    if suppress {
                        // Silence, not absence (#79). This leg's clock is the
                        // amount of PCM its socket has swallowed — Deepgram
                        // stamps every segment from it — so withholding a
                        // chunk does not skip a second, it *deletes* one, and
                        // every word after it is stamped a second early.
                        // Suppression's effect survives the change because
                        // silence transcribes as nothing; what it stops
                        // costing is time. Feeding zeros also keeps the socket
                        // from going quiet through sustained far-end speech,
                        // which is the audio-starvation half of #70.
                        hush.clear();
                        hush.resize(mono.len(), 0);
                        h.write_pcm(&hush);
                    } else {
                        h.write_pcm(&Downmixer::to_i16(&mono));
                    }
                }
            }
        }

        if !moved {
            if stop.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(IDLE_POLL);
        }
        seq = seq.wrapping_add(1);
    }
    let _ = seq;

    if let Some(gate) = &stt.echo_gate {
        let (assessed, suppressed) = gate.stats();
        if suppressed > 0 {
            // Once, at the end — CAP-11's metric, and the honest nudge: the
            // gate working hard means the user is on speakers.
            eprintln!(
                "  echo gate  : withheld {suppressed}/{assessed} mic chunks from \
                 transcription (speakers detected — headphones transcribe better)"
            );
        }
    }

    wal.flush().map_err(|e| format!("flush failed: {e}"))?;
    // Finalize stamps `ended_at_ms`. Without it every cleanly-ended meeting
    // looks crashed and reappears in the recovery list forever, which trains
    // the user to ignore the one prompt that matters.
    wal.finalize()
        .map_err(|e| format!("finalize failed: {e}"))?;
    // Read after the drain, so this is the final count rather than a snapshot
    // taken while the taps were still delivering. Both legs are summed: the
    // caller's field is one number, and either leg falling behind means the
    // same thing about this machine.
    Ok((
        sys_written,
        mic_written,
        sys.dropped_frames() + mic.dropped_frames(),
    ))
}

/// Append finalized segments to the session's `stt.jsonl`.
///
/// Separate from the pump because a transcript arriving after the audio has
/// stopped is normal — the provider is still finalizing when capture ends.
pub fn append_segments(dir: &Path, segments: &[TranscriptSegment]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("stt.jsonl"))?;
    for (i, s) in segments.iter().enumerate() {
        let rec = SttRecord {
            seq: i as u64,
            t0_ms: s.start_ms,
            t1_ms: s.end_ms,
            text: s.text.clone(),
            audio_byte_offset: 0,
        };
        writeln!(
            f,
            "{}",
            serde_json::to_string(&rec)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        )?;
    }
    f.sync_data()
}

#[cfg(test)]
mod pump_clock_tests {
    //! Where the two legs' clocks meet: the pump.
    //!
    //! A segment's `start_ms` is Deepgram's connection-relative time, and
    //! Deepgram's clock is *how much PCM the socket has been fed* — `replay.rs`
    //! says so in as many words. So anything the pump declines to feed a leg is
    //! time deleted from that leg's timeline, and the echo gate declined about
    //! a third of a real 30.5-minute meeting: its mic transcript ended at 22:01
    //! against the system leg's 30:31 (#79).
    //!
    //! Nothing crossed the legs before these tests. `fotw-stt`'s clock suite is
    //! unit math, the gate's own suite counts verdicts and stops there, and
    //! `mic_stt.rs` hand-writes `start_ms` — so the one question that mattered,
    //! *what does suppression do to the downstream clock*, was never asked.

    use super::*;
    use fotw_audio::SampleFormat;

    const CAPTURE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    /// What both legs are resampled to before a provider sees them.
    const FEED_RATE: u64 = 16_000;
    /// Long enough for the gate to warm up, engage and hold; short enough that
    /// the whole fixture fits in one ring without dropping.
    const SECONDS: usize = 8;

    fn capture_format() -> StreamFormat {
        StreamFormat::new(CAPTURE_RATE, CHANNELS, SampleFormat::F32)
    }

    /// A leg's provider connection, standing in for the socket and remembering
    /// what it swallowed. Fed samples are the clock; a write that is all zeros
    /// is what suppression is supposed to look like now.
    #[derive(Default)]
    struct CountingFeed {
        samples: AtomicU64,
        writes: AtomicU64,
        silent_writes: AtomicU64,
    }

    impl PcmFeed for CountingFeed {
        fn write_pcm(&self, pcm: &[i16]) {
            self.samples.fetch_add(pcm.len() as u64, Ordering::Relaxed);
            self.writes.fetch_add(1, Ordering::Relaxed);
            if pcm.iter().all(|s| *s == 0) {
                self.silent_writes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Deterministic speech-like audio: broadband noise under a syllable-paced
    /// envelope, mono at the capture rate.
    ///
    /// Noise under an aperiodic envelope for the reason `fotw-pipeline`'s gate
    /// suite documents at length — a periodic signal correlates with itself, so
    /// a tone would engage the gate for a reason a room never supplies.
    fn speech_like(seed: u32, n: usize) -> Vec<f32> {
        let mut carrier = seed.wrapping_mul(2_654_435_761).max(1);
        let mut envelope = seed.wrapping_mul(747_796_405).wrapping_add(1);
        let xorshift = |s: &mut u32| {
            *s ^= *s << 13;
            *s ^= *s >> 17;
            *s ^= *s << 5;
            *s
        };
        let mut level = 0.5f32;
        let mut target = 0.5f32;
        (0..n)
            .map(|i| {
                // A new loudness target every ~150 ms with a slow glide toward
                // it: deep, aperiodic, syllable-paced swings.
                if i % (CAPTURE_RATE as usize * 150 / 1_000) == 0 {
                    target = xorshift(&mut envelope) as f32 / u32::MAX as f32;
                }
                level += 0.0007 * (target - level);
                let swung = 0.03 + 0.97 * level.clamp(0.0, 1.0).powi(2);
                let noise = (xorshift(&mut carrier) as f32 / u32::MAX as f32) * 2.0 - 1.0;
                swung * 0.3 * noise
            })
            .collect()
    }

    /// The mic on speakers: a delayed, quieter copy of what is playing. Enough
    /// to make the gate engage, which is all this fixture is for — the room
    /// modelling that derives the gate's thresholds lives in its own suite.
    fn echoed(source: &[f32], lag: usize) -> Vec<f32> {
        (0..source.len())
            .map(|i| {
                i.checked_sub(lag)
                    .and_then(|j| source.get(j))
                    .copied()
                    .unwrap_or(0.0)
                    * 0.3
            })
            .collect()
    }

    fn interleave(mono: &[f32]) -> Vec<f32> {
        mono.iter().flat_map(|s| [*s, *s]).collect()
    }

    /// What each leg's socket was handed, once the pump had drained both rings.
    struct FedClocks {
        system_samples: u64,
        mic_samples: u64,
        mic_writes: u64,
        mic_silent_writes: u64,
    }

    /// A root nobody else in this binary will touch. The counter matters:
    /// these tests run in parallel and two of them drive the same gated
    /// fixture, so a name built from the test's role alone has one clearing
    /// the directory out from under the other's open WAL.
    fn tmp_root(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("fotwd-pump-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temp root");
        root
    }

    /// Drive both legs through the real pump with prefilled rings.
    ///
    /// The stop latch is set before the first iteration, so the loop drains
    /// what is there and returns: no threads, no timers, byte-identical every
    /// run. The audio is the coupled case the gate exists for — the mic hears
    /// the speakers — so `gated` decides whether a third of the mic leg gets
    /// judged echo.
    fn drive_both_legs(gated: bool) -> FedClocks {
        let root = tmp_root(if gated { "gated" } else { "open" });
        let wal = SessionWal::create(&root, CAPTURE_RATE, CHANNELS).expect("a session");

        let mono = speech_like(3, CAPTURE_RATE as usize * SECONDS);
        let system = interleave(&mono);
        let mic = interleave(&echoed(&mono, 40 * CAPTURE_RATE as usize / 1_000));

        let (mut sys_prod, sys_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
        let (mut mic_prod, mic_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
        assert_eq!(
            sys_prod.push_block(&system),
            system.len(),
            "the fixture has to fit the ring or the test measures drops instead"
        );
        assert_eq!(mic_prod.push_block(&mic), mic.len());

        let sys_feed = Arc::new(CountingFeed::default());
        let mic_feed = Arc::new(CountingFeed::default());
        let feeds = SttFeeds {
            system: Some(Arc::clone(&sys_feed) as Arc<dyn PcmFeed>),
            mic: Some(Arc::clone(&mic_feed) as Arc<dyn PcmFeed>),
            echo_gate: gated.then(|| fotw_pipeline::echo::EchoGate::new(FEED_RATE as u32)),
        };

        let stop = AtomicBool::new(true);
        pump_loop(
            wal,
            sys_cons,
            mic_cons,
            capture_format(),
            Some(capture_format()),
            feeds,
            &stop,
        )
        .expect("the pump drains cleanly");
        let _ = std::fs::remove_dir_all(&root);

        FedClocks {
            system_samples: sys_feed.samples.load(Ordering::Relaxed),
            mic_samples: mic_feed.samples.load(Ordering::Relaxed),
            mic_writes: mic_feed.writes.load(Ordering::Relaxed),
            mic_silent_writes: mic_feed.silent_writes.load(Ordering::Relaxed),
        }
    }

    /// The reported bug, at the seam it is made at.
    ///
    /// The gate suppresses most of this mic leg, and both legs must still come
    /// out of the pump having been fed the same amount of audio. A mic leg fed
    /// less than the system leg does not merely lose the suppressed words — it
    /// reports every word *after* them at the wrong time, and the error
    /// accumulates for the length of the meeting.
    #[test]
    fn suppressing_the_mic_leg_does_not_move_its_clock_off_the_system_legs() {
        let fed = drive_both_legs(true);

        let skew = fed.system_samples.abs_diff(fed.mic_samples) * 1_000 / FEED_RATE;
        assert!(
            skew <= 100,
            "the legs' fed-PCM clocks are {skew} ms apart after {SECONDS}s \
             (system fed {} samples, mic {})",
            fed.system_samples,
            fed.mic_samples
        );

        // And the gate really did work on this fixture — otherwise the
        // agreement above is the agreement of two ungated legs, which proves
        // nothing about suppression.
        assert!(
            fed.mic_silent_writes * 3 >= fed.mic_writes,
            "the fixture never engaged the gate: {}/{} mic chunks fed as silence",
            fed.mic_silent_writes,
            fed.mic_writes
        );
    }

    /// Suppression costs the mic leg words, never samples.
    ///
    /// The same audio with the gate off is the control: the leg's fed-sample
    /// count — its clock — must be identical either way, and only the content
    /// of the suppressed chunks may differ.
    #[test]
    fn suppression_does_not_shrink_the_mic_legs_fed_sample_count() {
        let gated = drive_both_legs(true);
        let open = drive_both_legs(false);

        assert_eq!(
            gated.mic_samples, open.mic_samples,
            "the gate changed how much audio the mic socket was fed"
        );
        assert_eq!(
            gated.mic_writes, open.mic_writes,
            "the gate changed how many chunks the mic socket saw"
        );
        assert!(
            gated.mic_silent_writes > 0,
            "the gate never engaged, so this proves nothing"
        );
        assert_eq!(
            open.mic_silent_writes, 0,
            "ungated mic audio must not arrive as silence"
        );
    }

    /// Ring drops were counted and then thrown away: the pump returned a
    /// hardcoded zero, so mic frames lost at a full ring reached no field, no
    /// log line and no human (#79).
    #[test]
    fn frames_dropped_at_a_full_ring_reach_the_pumps_caller() {
        let root = tmp_root("drops");
        let wal = SessionWal::create(&root, CAPTURE_RATE, CHANNELS).expect("a session");

        // A ring far smaller than the block the audio thread hands it: the
        // producer takes what fits and drops the rest, which is the whole
        // contract that makes the real-time side non-blocking.
        let (mut sys_prod, sys_cons) = AudioRing::with_capacity_frames(4_096);
        let (mut mic_prod, mic_cons) = AudioRing::with_capacity_frames(4_096);
        let block = vec![0.25f32; 6_000];
        sys_prod.push_block(&block);
        mic_prod.push_block(&block);
        let expected = 2 * (block.len() - 4_096) as u64;

        let stop = AtomicBool::new(true);
        let (_, _, dropped) = pump_loop(
            wal,
            sys_cons,
            mic_cons,
            capture_format(),
            Some(capture_format()),
            SttFeeds {
                system: None,
                mic: None,
                echo_gate: None,
            },
            &stop,
        )
        .expect("the pump drains cleanly");
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(dropped, expected, "the shortfall both rings counted");
    }
}

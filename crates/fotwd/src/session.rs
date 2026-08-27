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
use std::time::{Duration, Instant};

use fotw_audio::{AudioTap, CaptureTimestamp, FrameFlags, FrameSink, StreamFormat, TapError};
use fotw_pipeline::resample::{Downmixer, Resampler16k};
use fotw_pipeline::ring::{AudioRing, RingConsumer, RingProducer};
use fotw_pipeline::wal::{SessionWal, SttRecord, TrackFormat};
use fotw_stt::deepgram::DeepgramConfig;
use fotw_stt::{DeepgramStream, DeepgramStreamConfig, Source, StreamEvent, TranscriptSegment};

/// Ring capacity per leg, in samples. Ten seconds at 48 kHz stereo.
const RING_SAMPLES: usize = 48_000 * 2 * 10;

/// How long the pump waits when both rings are empty.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// How long the session waits for both taps' first buffer before it gives up
/// on anchoring the two legs to one clock (#86).
///
/// See [`anchor_legs`] for why there is a wait here and why it is nearly always
/// over on the first look. A quarter of a second is far longer than a working
/// device needs and far shorter than [`crate::recording::READY_DEADLINE`], so
/// it can only ever delay a session that was already about to be reported as
/// having a dead leg.
pub const ANCHOR_DEADLINE: Duration = Duration::from_millis(250);

/// How long the session waits for the taps to close (#85).
///
/// A healthy `stop()` returns in microseconds. This is the point at which the
/// session stops believing the device — the same judgement
/// [`crate::recording::READY_DEADLINE`] makes about `start()`, and for the
/// same reason: a Core Audio HAL that still believes a dead client holds the
/// device blocks rather than failing, and nothing in this process can cancel
/// the syscall it is stuck in.
pub const CLOSE_DEADLINE: Duration = Duration::from_secs(10);

/// How long the pump keeps draining after the taps have been asked to close
/// (#85).
///
/// A ring holds ten seconds of audio and the pump empties it in milliseconds,
/// so this is not a budget the ordinary path spends: it is the point at which
/// the session stops waiting for a tap that never really stopped.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// How long the session then waits for the pump's counts (#85).
///
/// The backstop for the blocking points no latch can reach: a `write(2)` into
/// a filesystem that has stopped answering, an `fsync` on a disk that has gone
/// away. Generous, because a long meeting's final flush is real work, and
/// finite, because `pump.join()` used to have no clock at all.
pub const PUMP_JOIN_DEADLINE: Duration = Duration::from_secs(30);

/// How long the `Finishing` state may last (#85).
///
/// #77 named that state and drew it honestly; it did not give it an end. The
/// tail of [`run_with_control`] is three blocking calls on a tokio worker —
/// `system.stop()`, `mic.stop()` and the wait on the pump — and a device that
/// would not close, or a pump that would not come back, held the recorder's
/// slot until the daemon restarted. These are that state's clock.
///
/// One field per step rather than one budget for the lot, because the three
/// fail differently and only the middle one is repairable in-process:
/// [`close`](Self::close) gives up on the device, [`drain`](Self::drain) is
/// what makes the pump *able* to return, and [`join`](Self::join) is what
/// happens when it does not anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinishDeadlines {
    /// How long the session waits for the taps to close.
    ///
    /// On expiry the taps are abandoned open and the session says so: only
    /// the user can clear a wedged HAL.
    pub close: Duration,
    /// How long the pump then keeps draining what the rings still hold.
    ///
    /// Whatever is left when it expires is audio that will exist nowhere, so
    /// the session says so on the one degradation channel with a reader on the
    /// other end (#79). The session is still finalized — that is the point.
    pub drain: Duration,
    /// How long the session then waits for the pump to hand back its counts.
    ///
    /// Expiry detaches the pump thread and fails the session; see
    /// [`run_with_control`] for what that leaves behind, and why it is a
    /// last resort rather than the design.
    pub join: Duration,
}

impl Default for FinishDeadlines {
    fn default() -> Self {
        Self {
            close: CLOSE_DEADLINE,
            drain: DRAIN_DEADLINE,
            join: PUMP_JOIN_DEADLINE,
        }
    }
}

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
    /// What the system tap delivered. Required, so always present: a session
    /// whose system tap refuses to start never gets this far.
    pub system_buffers: LegBuffers,
    /// What the mic tap delivered, or `None` when there was no mic leg at all
    /// — no input device, or a tap that refused to start. Distinct from
    /// `Some(LegBuffers::default())`, which is a mic tap that started and then
    /// never fired: the first is a machine without a microphone, the second is
    /// a microphone that is not working.
    pub mic_buffers: Option<LegBuffers>,
    /// Samples the ring dropped because the pump fell behind.
    pub dropped_samples: u64,
    /// Finalized transcript segments.
    pub segments: Vec<TranscriptSegment>,
}

impl SessionOutcome {
    /// Whether any audible audio was captured at all.
    ///
    /// # Why either leg is enough
    ///
    /// This is the one question with a single answer, and the only honest one
    /// is *did anything reach the disk*. Neither leg's silence proves a
    /// failure on its own, because which leg is legitimately quiet is a fact
    /// about the meeting, not about the machine: a note to self has no far end
    /// to capture, and a meeting where the user only listened has a quiet mic.
    /// A rule that failed on either leg would call both of those a broken
    /// recording. Only *both* legs silent means nothing was captured — which
    /// is the case the CLI's permission guidance exists for.
    ///
    /// The sharper question — *which* leg was quiet, and was that expected —
    /// needs the per-leg counts, which is why [`system_buffers`] and
    /// [`mic_buffers`] are reported rather than summed (#81). Summing them
    /// destroys exactly the disagreement worth reading.
    ///
    /// [`system_buffers`]: Self::system_buffers
    /// [`mic_buffers`]: Self::mic_buffers
    #[must_use]
    pub const fn captured_audio(&self) -> bool {
        matches!(self.system_buffers.audio(), LegAudio::Audible)
            || matches!(self.mic_buffers, Some(b) if matches!(b.audio(), LegAudio::Audible))
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

/// What one capture leg's tap delivered, counted on the audio thread.
///
/// Per leg rather than summed, because "the system leg was live" and "the mic
/// leg was live" are different questions and the interesting answer is the one
/// where they disagree — a dead microphone beside a working system tap is
/// invisible in a total (#81).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegBuffers {
    /// How many of those buffers were bit-exact digital silence.
    pub silent: u64,
    /// Buffers the tap delivered to its sink.
    pub total: u64,
}

impl LegBuffers {
    /// What these counts say about the leg.
    #[must_use]
    pub const fn audio(&self) -> LegAudio {
        if self.total == 0 {
            LegAudio::Nothing
        } else if self.silent >= self.total {
            LegAudio::Silent
        } else {
            LegAudio::Audible
        }
    }
}

/// What a capture leg's buffers said about it.
///
/// Three states, not a bool: "the tap never fired" and "the tap fired and
/// every buffer was silence" are different faults with different causes, and
/// collapsing them is how a dead device reads as a quiet room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegAudio {
    /// The tap started and then delivered no buffer at all. A stalled IOProc,
    /// not a quiet one.
    Nothing,
    /// Every buffer it delivered was bit-exact digital silence — a muted or
    /// dead device, a denied permission (macOS answers a denial with silence
    /// rather than an error), or genuinely nothing to hear.
    Silent,
    /// At least one buffer carried audio. This leg was live.
    Audible,
}

/// The counters one leg's [`RingSink`] bumps on the audio thread, and the
/// session's own handle on them.
///
/// A type rather than two loose `Arc`s at the construction site, because #81
/// was precisely that: the system leg's sink was handed clones of the pair the
/// session read, and the mic leg's was handed a pair constructed inline that
/// nobody else held. Building the sink *from* the counters leaves nowhere to
/// spell the orphaned version.
#[derive(Clone, Debug)]
struct LegCounters {
    silent: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    /// This leg's t0: the host-clock nanosecond of its **first** buffer, and
    /// [`NO_BUFFER`](Self::NO_BUFFER) until one arrives (#86).
    ///
    /// # Why only the first
    ///
    /// The ring is `RingBuffer<f32>` — samples, no side channel — so a stamp
    /// per buffer would need a second queue for the pump to correlate against,
    /// and nothing downstream would read it. Per-leg t0 is the whole of what
    /// lining the two legs up requires: within a leg the device's own frame
    /// counter is the exact measure of how much audio exists, and
    /// `CaptureTimestamp::is_monotonic_after` already asserts the two clocks
    /// agree buffer to buffer. Mid-session *drift* between the two devices is
    /// real and is deliberately not detected here; when something wants to act
    /// on it, this field grows a companion — the ring does not.
    first_host_ns: Arc<AtomicU64>,
}

impl Default for LegCounters {
    fn default() -> Self {
        Self {
            silent: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            first_host_ns: Arc::new(AtomicU64::new(Self::NO_BUFFER)),
        }
    }
}

impl LegCounters {
    /// [`first_host_ns`](Self::first_host_ns) before the tap has fired.
    ///
    /// Not zero: zero is a legitimate reading, taken by any tap that delivers
    /// its first buffer in the same nanosecond the process-wide epoch was, and
    /// a sentinel that a real value can collide with is a clock that silently
    /// un-anchors itself.
    const NO_BUFFER: u64 = u64::MAX;

    /// A sink for `producer` that reports to these counters.
    fn sink(&self, producer: RingProducer) -> Box<dyn FrameSink> {
        Box::new(RingSink {
            producer,
            counters: self.clone(),
        })
    }

    /// This leg's t0 on the shared host clock, or `None` before its first
    /// buffer.
    fn t0_ns(&self) -> Option<u64> {
        match self.first_host_ns.load(Ordering::Relaxed) {
            Self::NO_BUFFER => None,
            ns => Some(ns),
        }
    }

    /// Read both counters. Not atomic *together*, which costs at most one
    /// buffer of skew and only ever while the tap is still running — the
    /// session reads this after `stop()`, so its snapshot is settled.
    fn snapshot(&self) -> LegBuffers {
        LegBuffers {
            silent: self.silent.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
        }
    }
}

/// A sink that copies into a ring and returns. Nothing else may happen here.
struct RingSink {
    producer: RingProducer,
    counters: LegCounters,
}

impl FrameSink for RingSink {
    fn on_frames(&mut self, pcm: &[f32], ts: CaptureTimestamp, flags: FrameFlags) {
        // Counted here, at the tap, and nowhere downstream: this is what the
        // *device* delivered. The echo gate's suppression (#79) rewrites what
        // the mic's STT feed is handed, far below this line, so a mic that
        // spends a meeting being suppressed still counts every buffer it heard
        // as audible — which is the whole point of counting here.
        self.counters.total.fetch_add(1, Ordering::Relaxed);
        if flags.contains(FrameFlags::SILENT) {
            self.counters.silent.fetch_add(1, Ordering::Relaxed);
        }
        // This leg's t0 on the process-wide host clock, which is the one thing
        // in a `CaptureTimestamp` that means anything across two devices —
        // `frames.rs` calls it "what makes the mic leg and the system leg line
        // up: seam rule 3". It used to be discarded right here, which is why
        // the two legs had no shared epoch and any path that withheld audio
        // from one of them skewed its timeline for good (#86). A predictable
        // branch and a relaxed store: still no allocation, no lock and no log
        // on this thread (CAP-04).
        if self.counters.first_host_ns.load(Ordering::Relaxed) == LegCounters::NO_BUFFER {
            self.counters
                .first_host_ns
                .store(ts.host_ns, Ordering::Relaxed);
        }
        // The return value is deliberately ignored: a short write means the
        // pump is behind, and retrying on a real-time thread is blocking by
        // another name. The shortfall is counted for the pump to surface.
        let _ = self.producer.push_block(pcm);
    }

    fn on_error(&mut self, _e: TapError) {}
}

/// Where each capture leg's own audio zero sits on the session clock (#86).
///
/// Both zero is not a missing answer — it is the answer for two taps that woke
/// together, and the answer whenever the legs cannot be compared at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LegAnchors {
    /// Milliseconds from session t0 to the system leg's first sample.
    system_ms: u64,
    /// Milliseconds from session t0 to the mic leg's first sample.
    mic_ms: u64,
}

/// Put the two legs on one epoch: session t0 is the earlier first buffer, and
/// each leg's anchor is its distance from it.
///
/// Both taps are required. A leg with no t0 is a tap that has not delivered a
/// buffer, and there is nothing to line the other one up against; anchoring the
/// one we can see would invent a relationship rather than measure one, so the
/// session falls back to what it did before #86 — both legs on their own
/// fed-PCM clock. Whichever leg *is* live is unaffected either way: it is
/// alone, and a single leg's timeline is internally consistent whatever its
/// epoch.
fn leg_anchors(system_t0_ns: Option<u64>, mic_t0_ns: Option<u64>) -> LegAnchors {
    let (Some(system), Some(mic)) = (system_t0_ns, mic_t0_ns) else {
        return LegAnchors::default();
    };
    let session_t0 = system.min(mic);
    LegAnchors {
        system_ms: (system - session_t0) / 1_000_000,
        mic_ms: (mic - session_t0) / 1_000_000,
    }
}

/// Wait for both legs' first buffer, then place them on one epoch (#86).
///
/// # Why the session waits at all
///
/// A leg's anchor has to be in its clock *before* the first PCM reaches its
/// socket. Deepgram stamps every segment from how much audio that socket has
/// swallowed, so an anchor applied afterwards leaves a prefix of the meeting on
/// the old epoch — a race, and a race in a clock is the class of bug this whole
/// change exists to close. Nothing above the tap can be told when the first
/// buffer landed except by looking, so the session looks.
///
/// It is cheap: `start()` has already returned on both taps, a live device
/// delivers within one IOProc period, and creating the WAL above this line
/// usually costs more than that. The deadline is not a budget for the ordinary
/// case — it is the point at which the session stops waiting for a device that
/// has not woken up, and records the meeting unanchored rather than making the
/// user wait on a clock correction.
async fn anchor_legs(system: &LegCounters, mic: &LegCounters, deadline: Duration) -> LegAnchors {
    /// How often the session looks. Well under an IOProc period, so in practice
    /// this loop tests its condition once and returns.
    const POLL: Duration = Duration::from_millis(1);

    let until = Instant::now() + deadline;
    loop {
        let (system_t0_ns, mic_t0_ns) = (system.t0_ns(), mic.t0_ns());
        if (system_t0_ns.is_some() && mic_t0_ns.is_some()) || Instant::now() >= until {
            return leg_anchors(system_t0_ns, mic_t0_ns);
        }
        tokio::time::sleep(POLL).await;
    }
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
/// lands twice — once diarized on the system leg, once labeled `me` — with ASR
/// wording drift between the copies and seconds of offset between the two
/// spans, for the reasons [`DEDUPE_WINDOW_MS`] sets out. The audio gate
/// (CAP-11 v1) removes what it can before transcription; this pass removes
/// what leaks, because in the text domain the duplication is trivially visible
/// no matter what the room did to the waveform. Precedent: Descript's "mic
/// bleed" fix — text-only removal, audio untouched. The system copy always
/// wins: it is the clean, diarized feed.
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

/// Span padding for the containment window.
///
/// The number is unchanged since it was measured; what it covers is not
/// (#89). It was written for a multi-second disagreement between the two legs'
/// clocks. #79 stopped the echo gate stealing time from the mic leg's, and #86
/// put both legs on one session epoch — [`anchor_legs`] — so a clock
/// disagreement is now the thing this padding does *not* have to cover.
///
/// What is left is a genuine offset between the two spans carrying the same
/// words, and it is still seconds:
///
/// * **Segmentation.** The legs are two independent Deepgram connections that
///   each endpoint where they hear a pause, so the same passage is cut
///   differently on each. The live capture in `tests/cross_leg_dedupe.rs` has
///   one mic segment at 2 000–17 500 ms against three system segments spanning
///   0–16 000 ms. This is the dominant term, and it is the reason the window
///   has to reach *outside* the mic segment's own span at all.
/// * **The room.** The mic's copy is the system audio after rendering, playout
///   and flight, so it really did happen later. Tens to a few hundred ms.
/// * **The unanchored fallback.** [`leg_anchors`] answers `{0, 0}` whenever
///   either tap has not delivered a buffer by [`ANCHOR_DEADLINE`], which puts
///   both legs back on their own fed-PCM clocks and hands the sub-second
///   tap-start offset back.
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

/// The latches a caller outside the session holds, and the clock on its end.
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
    /// How long the wind-down may take before the session stops waiting on
    /// the device (#85). The default is right for a laptop; a test that would
    /// otherwise sit out a ten-second drain names its own.
    pub deadlines: FinishDeadlines,
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
            deadlines: FinishDeadlines::default(),
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
    let deadlines = control.deadlines;
    // One pair per leg, both held here. The mic's used to be constructed
    // inline inside its sink, so its liveness was unobservable (#81).
    let sys_counters = LegCounters::default();
    let mic_counters = LegCounters::default();

    // Every allocation happens here, before anything real-time is running.
    let (sys_prod, sys_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
    let (mic_prod, mic_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);

    let sys_format = system
        .start(sys_counters.sink(sys_prod))
        .map_err(|e| format!("could not start the system tap: {e}"))?;

    let mic_format = mic
        .as_mut()
        .and_then(|t| t.start(mic_counters.sink(mic_prod)).ok());

    // A format per leg, because the two taps are two devices: the system tap
    // is 48 kHz stereo and the mic is usually mono. Recording the system's
    // count for both is #80 — the encoder then reads the mono mic WAL as
    // stereo and archives it at half its real length, at 2× speed.
    let wal = SessionWal::create_with_formats(
        root,
        TrackFormat::new(sys_format.sample_rate_hz, sys_format.channels),
        mic_format.map(|f| TrackFormat::new(f.sample_rate_hz, f.channels)),
    )
    .map_err(|e| format!("could not create the session: {e}"))?;
    let dir = wal.dir().to_path_buf();
    let started_at_ms = wal.manifest().started_at_ms;

    // Both legs on one epoch, settled before either socket exists (#86) — see
    // `anchor_legs` for why this cannot wait until after they do.
    //
    // Only when both are actually being transcribed: a lone leg has nothing to
    // be lined up with — its timeline is internally consistent whatever its
    // epoch — so waiting on a tap nobody will read would cost the user
    // readiness for nothing.
    let both_legs_transcribed = mic_format.is_some()
        && matches!(&transcription, Transcription::Deepgram(legs) if legs.mic.is_some());
    let anchors = if both_legs_transcribed {
        anchor_legs(&sys_counters, &mic_counters, ANCHOR_DEADLINE).await
    } else {
        LegAnchors::default()
    };

    // The STT side, if configured. `write` is non-blocking, so the pump can
    // feed it without ever waiting on the network.
    // One stream per leg. The mic stream is additionally gated on the mic tap
    // having actually started: a paid connection for a device that is not
    // there would be fed nothing and still billed for the socket.

    let (sys_stt, sys_events, mic_stt, mic_events) = match transcription {
        Transcription::Disabled => (None, None, None, None),
        Transcription::Deepgram(mut legs) => {
            // The one place a leg's anchor is spelled. From here down it is the
            // stream's own property, applied at connection zero and carried
            // across every reconnect.
            legs.system.session_offset_ms = anchors.system_ms;
            let (s, s_rx) = DeepgramStream::open(*legs.system);
            let (m, m_rx) = match legs.mic {
                Some(mut cfg) if mic_format.is_some() => {
                    cfg.session_offset_ms = anchors.mic_ms;
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
    //
    // Its counts come back over a channel rather than off the `JoinHandle`:
    // `join()` blocks with no deadline, and this is an `async fn` on the
    // multi-threaded runtime, so a pump that never returned held the
    // recorder's slot until the daemon restarted — and, because a blocked
    // worker stops driving the runtime's time source, took every `tokio::time`
    // future in the process with it (#85). A channel can be waited on with a
    // clock; the handle is dropped, so a pump still inside a syscall nothing
    // can cancel is detached rather than waited for.
    let (pump_done, pump_counts) = tokio::sync::oneshot::channel();
    let _pump = std::thread::spawn(move || {
        let counts = pump_loop(
            wal,
            sys_cons,
            mic_cons,
            sys_format,
            mic_format,
            feeds,
            PumpStop {
                stopped: &pump_stop,
                drain: deadlines.drain,
            },
        );
        // A receiver that has gone away is the join deadline having expired.
        // There is nobody left to tell.
        let _ = pump_done.send(counts);
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
    //
    // Off the runtime and under a deadline (#85): closing a tap is a blocking
    // call into the platform, and the Core Audio HAL that blocks in `start()`
    // — the one `recording::READY_DEADLINE` exists for — blocks in `stop()`
    // for the same reason, with nothing in this process able to cancel it.
    // When the deadline expires the taps are left to the blocking
    // pool: a device we cannot close is not a reason to hold the recorder,
    // and the frames it goes on delivering are the pump's deadline to bound.
    let closing = tokio::task::spawn_blocking(move || {
        let _ = system.stop();
        if let Some(t) = mic.as_mut() {
            let _ = t.stop();
        }
    });
    let taps_closed = matches!(
        tokio::time::timeout(deadlines.close, closing).await,
        Ok(Ok(()))
    );
    stop.store(true, Ordering::Release);

    let PumpCounts {
        system_samples,
        mic_samples,
        dropped_samples,
        abandoned_samples,
    } = match tokio::time::timeout(deadlines.join, pump_counts).await {
        Ok(Ok(counts)) => counts?,
        // The sender went with the thread.
        Ok(Err(_)) => return Err("pump panicked".to_string()),
        Err(_) => {
            // The one path that really does strand a session, and it says so
            // rather than filing it under "the recovery path will get it".
            // It will not: `promote::pending` takes a directory only when the
            // manifest has both `ended_at_ms` and `claim`, and this one has
            // neither — `finalize` never ran, and `claim` is written by
            // `retention::promote_session`, which is downstream of here.
            return Err(format!(
                "the pump did not hand back its counts within {:?} of being \
                 told to stop, so the recorder has been freed and the session \
                 at {} abandoned mid-write. Its manifest has neither \
                 `ended_at_ms` nor a `claim`, both of which `promote::pending` \
                 requires, so nothing will collect it: the audio is on disk \
                 and has to be imported by hand. A pump stuck this long is \
                 stuck in a write this process cannot cancel — check the disk \
                 the library lives on.",
                deadlines.join,
                dir.display()
            ));
        }
    };

    // Read after the taps are stopped, so these are final counts rather than a
    // snapshot taken mid-meeting. A tap the deadline gave up on is the
    // exception, and it is named below rather than quietly folded in.
    let system_buffers = sys_counters.snapshot();
    let mic_buffers = mic_format.is_some().then(|| mic_counters.snapshot());

    // A leg that captured nothing audible is a degraded meeting, so it goes
    // where degradation goes: the one channel with a reader on the other end
    // (#79). Per leg and named, because the remedies are different — a silent
    // system leg is a screen-recording grant, a silent mic leg is a muted or
    // dead microphone — and because `captured_audio()` deliberately no longer
    // fails on one silent leg. Without these lines a denied system grant on a
    // machine with a working mic would pass in complete silence, which is the
    // failure this project exists to make impossible.
    for (leg, buffers) in [("system", Some(system_buffers)), ("mic", mic_buffers)] {
        let Some(buffers) = buffers else { continue };
        match buffers.audio() {
            LegAudio::Audible => {}
            LegAudio::Nothing => control.errors.record(format!(
                "capture ({leg}): the tap started and then delivered no audio \
                 at all — the device stalled rather than went quiet"
            )),
            LegAudio::Silent => control.errors.record(format!(
                "capture ({leg}): every one of {} buffers was digitally silent \
                 — {}",
                buffers.total,
                if leg == "mic" {
                    "a muted, denied or dead microphone, so this meeting has \
                     none of the near end"
                } else {
                    "either nothing was playing, or system-audio capture was \
                     denied (macOS answers a denial with silence, not an error)"
                }
            )),
        }
    }

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

    // The same hole from the other cause: the tap went on delivering after it
    // was told to stop, and rather than drain it forever the pump gave up and
    // finalized what it had (#85). Reported for the same reason as a ring
    // drop — this audio exists nowhere — and the meeting is otherwise intact,
    // which is precisely why the pump ends itself instead of being abandoned.
    if abandoned_samples > 0 {
        control.errors.record(format!(
            "capture: {abandoned_samples} samples were still in the ring when \
             the {:?} drain deadline expired — the tap kept delivering after \
             it was stopped, so the last moments of this meeting are missing",
            deadlines.drain
        ));
    }

    // Last, so it is what `SttErrors::latest()` shows: it is the only entry
    // here with a remedy the user can carry out, and the device is still open.
    if !taps_closed {
        control.errors.record(format!(
            "capture: the audio device did not close within {:?} and has been \
             left open. This is usually a Core Audio HAL that still believes a \
             dead client holds the device; it blocks rather than failing, and \
             nothing in this process can cancel it. Restart the audio daemon \
             with `sudo killall coreaudiod` (this briefly interrupts all \
             audio) before recording again.",
            deadlines.close
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
        system_buffers,
        mic_buffers,
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

/// How the pump is told to end, and how long it may take.
///
/// One type rather than two parameters, for the reason [`SttFeeds`] is one:
/// the pump's argument list is at clippy's limit. And because neither half
/// means anything alone — a latch with nothing bounding it is #85, and a
/// deadline with no latch is nothing at all.
struct PumpStop<'a> {
    /// Tripped by the session once the taps have been asked to close.
    stopped: &'a AtomicBool,
    /// How long the pump keeps draining after it *notices*, rather than after
    /// the store: the session cannot know when the pump will next look, so the
    /// clock has to start on the pump's side.
    drain: Duration,
}

/// A drain deadline the pump's tests are not meant to reach.
///
/// Named once so that a bare `Duration::from_secs(30)` beside a prefilled ring
/// cannot read like a tuning decision: every test that passes it is asking
/// about the *other* branch, where the rings empty first.
#[cfg(test)]
const GENEROUS_DRAIN: Duration = Duration::from_secs(30);

/// What one pump run moved, and what it did not.
struct PumpCounts {
    /// Interleaved samples written to the system leg.
    system_samples: u64,
    /// Interleaved samples written to the mic leg.
    mic_samples: u64,
    /// Samples the rings dropped because the pump fell behind.
    dropped_samples: u64,
    /// Samples still in the rings when the drain deadline expired, and zero
    /// on every clean stop (#85).
    abandoned_samples: u64,
}

/// Drain both rings until stopped, writing raw audio and feeding the provider.
fn pump_loop(
    mut wal: SessionWal,
    mut sys: RingConsumer,
    mut mic: RingConsumer,
    sys_format: StreamFormat,
    mic_format: Option<StreamFormat>,
    mut stt: SttFeeds,
    stop: PumpStop<'_>,
) -> Result<PumpCounts, String> {
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

    // Armed on the pass that first sees the latch, and the only thing that
    // ends this loop. The latch used to be read solely in the idle branch, so
    // a leg still delivering after `stop()` — a HAL that acknowledged a
    // teardown it never performed — left `moved` true on every pass and the
    // pump never looked at it again (#85).
    let mut drain_by: Option<Instant> = None;
    let mut abandoned = 0u64;

    loop {
        if drain_by.is_none() && stop.stopped.load(Ordering::Acquire) {
            drain_by = Some(Instant::now() + stop.drain);
        }

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

        match drain_by {
            // Still recording. Nothing to do but wait for more audio.
            None if !moved => std::thread::sleep(IDLE_POLL),
            None => {}
            // Draining, and there was nothing left to drain: the ordinary end
            // of every meeting, and the only exit this loop used to have.
            Some(_) if !moved => break,
            // Draining, and still behind when the deadline expired. What is
            // left in the rings is audio nobody will ever write — but the
            // finalize below still runs, which is the whole point: a session
            // left unfinalized is one `promote::pending` skips for good.
            Some(by) if Instant::now() >= by => {
                abandoned = (sys.slots() + mic.slots()) as u64;
                break;
            }
            Some(_) => {}
        }
    }

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
    Ok(PumpCounts {
        system_samples: sys_written,
        mic_samples: mic_written,
        dropped_samples: sys.dropped_frames() + mic.dropped_frames(),
        abandoned_samples: abandoned,
    })
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
    //!
    //! #86 generalises the answer. Feeding silence keeps the gate from stealing
    //! time, but it only works where the pump *has* the audio; a leg that never
    //! captured it in the first place has nothing to substitute. So the two
    //! legs now share an epoch taken from the host clock at the tap, and the
    //! tests below drive audio being withheld from a leg *upstream of the pump*
    //! and check that the two timelines still meet.

    use super::*;
    use fotw_audio::SampleFormat;
    use fotw_stt::SessionClock;

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

    /// What each leg's socket was handed, once the pump had drained both
    /// rings — and, separately, what each leg's *tap* delivered.
    struct FedClocks {
        system_samples: u64,
        mic_samples: u64,
        mic_writes: u64,
        mic_silent_writes: u64,
        mic_buffers: LegBuffers,
        /// Each leg's first buffer on the shared host clock, which is where
        /// its anchor comes from (#86).
        system_t0_ns: Option<u64>,
        mic_t0_ns: Option<u64>,
    }

    impl FedClocks {
        /// A leg's fed-PCM position at the end of the run, in milliseconds:
        /// what its provider's clock reads once it has heard everything.
        fn system_fed_ms(&self) -> u64 {
            self.system_samples * 1_000 / FEED_RATE
        }

        fn mic_fed_ms(&self) -> u64 {
            self.mic_samples * 1_000 / FEED_RATE
        }
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
        drive_both_legs_late(gated, 0)
    }

    /// [`drive_both_legs`], with the mic tap waking up `mic_late_ms` after the
    /// system tap (#86).
    ///
    /// A late tap is the general shape of the #79 class: its first buffer is
    /// stamped that much later on the shared host clock, and the audio from
    /// before it never reaches the ring at all. Both legs still stop at the
    /// same instant, so the last sample each socket swallowed is the same
    /// moment in the room — which is the thing the two clocks have to agree
    /// about.
    fn drive_both_legs_late(gated: bool, mic_late_ms: u64) -> FedClocks {
        let root = tmp_root(if gated { "gated" } else { "open" });
        let wal = SessionWal::create(&root, CAPTURE_RATE, CHANNELS).expect("a session");

        let mono = speech_like(3, CAPTURE_RATE as usize * SECONDS);
        let system = interleave(&mono);
        let mic_full = interleave(&echoed(&mono, 40 * CAPTURE_RATE as usize / 1_000));
        // Interleaved samples the mic tap was not yet awake for.
        let missed = (mic_late_ms as usize * CAPTURE_RATE as usize / 1_000) * CHANNELS as usize;
        let mic = &mic_full[missed.min(mic_full.len())..];

        let (sys_prod, sys_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
        let (mic_prod, mic_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);

        // Through the real sinks, in tap-sized buffers, rather than one bulk
        // `push_block`: the liveness counters live on that callback, and
        // routing the fixture past them is what lets a test downstream of the
        // gate say anything about them (#81).
        let sys_counters = LegCounters::default();
        let mic_counters = LegCounters::default();
        deliver(&mut *sys_counters.sink(sys_prod), &system, 0);
        deliver(
            &mut *mic_counters.sink(mic_prod),
            mic,
            mic_late_ms * 1_000_000,
        );
        // The sink swallows a short write by design, so the fit has to be
        // checked from the other end. Without this the fixture could silently
        // shrink and these tests would measure ring drops instead.
        assert_eq!(
            (sys_cons.dropped_frames(), mic_cons.dropped_frames()),
            (0, 0),
            "the fixture has to fit the ring or the test measures drops instead"
        );

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
            PumpStop {
                stopped: &stop,
                drain: GENEROUS_DRAIN,
            },
        )
        .expect("the pump drains cleanly");
        let _ = std::fs::remove_dir_all(&root);

        FedClocks {
            system_samples: sys_feed.samples.load(Ordering::Relaxed),
            mic_samples: mic_feed.samples.load(Ordering::Relaxed),
            mic_writes: mic_feed.writes.load(Ordering::Relaxed),
            mic_silent_writes: mic_feed.silent_writes.load(Ordering::Relaxed),
            mic_buffers: mic_counters.snapshot(),
            system_t0_ns: sys_counters.t0_ns(),
            mic_t0_ns: mic_counters.t0_ns(),
        }
    }

    /// Hand `pcm` to a sink the way a tap does: fixed-size buffers, a clock
    /// that advances, and a `SILENT` flag set from what is actually in the
    /// buffer — which is where the flag comes from on the real path too.
    ///
    /// `start_ns` is where this tap's first buffer lands on the shared host
    /// clock. The two legs pass different values for the same reason two real
    /// taps do: they are started in sequence and wake up when they wake up.
    fn deliver(sink: &mut dyn FrameSink, pcm: &[f32], start_ns: u64) {
        /// 5 ms of 48 kHz stereo, near the low end of a real IOProc's block.
        const BUFFER: usize = 480 * CHANNELS as usize;
        for (i, chunk) in pcm.chunks(BUFFER).enumerate() {
            let frames = (i * BUFFER / CHANNELS as usize) as u64;
            let mut flags = FrameFlags::empty();
            flags.set(FrameFlags::SILENT, chunk.iter().all(|s| *s == 0.0));
            sink.on_frames(
                chunk,
                CaptureTimestamp::new(
                    frames,
                    start_ns + frames * 1_000_000_000 / u64::from(CAPTURE_RATE),
                ),
                flags,
            );
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

    /// #79 meets #81: a suppressed mic leg is not a silent mic leg.
    ///
    /// The gate's suppression writes zeros into the mic's *STT feed*, and this
    /// fixture is the coupled case where it does that to most of the meeting.
    /// If the liveness counters lived anywhere downstream of that — in the
    /// pump, or off what the feed was handed — a user on speakers would come
    /// out of a working meeting with a microphone reported dead, which is the
    /// exact false positive #81's counters exist to avoid. They are read here
    /// *after* a fully gated run, so nothing between the tap and the socket
    /// gets to rewrite what the device delivered.
    #[test]
    fn suppression_cannot_make_a_working_microphone_look_silent() {
        let fed = drive_both_legs(true);

        let suppressed = 100.0 * fed.mic_silent_writes as f64 / fed.mic_writes as f64;
        assert!(
            suppressed > 50.0,
            "the gate withheld only {suppressed:.0}% of the mic feed, so this \
             proves nothing"
        );

        // The tap's own view, unmoved. The handful of silent buffers that do
        // land are the fixture's: `echoed` opens with `lag` of true zeros
        // before the delayed copy starts, which is a few dozen milliseconds
        // against a suppressed majority.
        assert_eq!(fed.mic_buffers.audio(), LegAudio::Audible);
        let silent = 100.0 * fed.mic_buffers.silent as f64 / fed.mic_buffers.total as f64;
        assert!(
            silent < 5.0,
            "the gate withheld {suppressed:.0}% of the mic feed and the leg's \
             own counters moved with it: {silent:.0}% of {} buffers read as \
             digital silence",
            fed.mic_buffers.total
        );
    }

    // -----------------------------------------------------------------------
    // The shared host clock (#86)
    // -----------------------------------------------------------------------

    /// Piece one: the `host_ns` `RingSink` used to throw away.
    ///
    /// Only the *first* buffer's, because per-leg t0 is the whole of what
    /// anchoring needs — see the note on [`LegCounters::first_host_ns`] for why
    /// per-buffer stamps were not built.
    #[test]
    fn a_leg_records_the_host_time_of_its_first_buffer_and_of_no_later_one() {
        let counters = LegCounters::default();
        assert_eq!(
            counters.t0_ns(),
            None,
            "a tap that has not delivered a buffer has no t0 to report"
        );

        let (producer, _consumer) = AudioRing::with_capacity_frames(RING_SAMPLES);
        let mut sink = counters.sink(producer);
        sink.on_frames(
            &[0.5, 0.5],
            CaptureTimestamp::new(0, 4_000_000),
            FrameFlags::empty(),
        );
        sink.on_frames(
            &[0.5, 0.5],
            CaptureTimestamp::new(1, 9_000_000),
            FrameFlags::empty(),
        );

        assert_eq!(counters.t0_ns(), Some(4_000_000));
    }

    /// Piece two: session t0 is the earlier leg's, and each leg's anchor is its
    /// distance from it.
    #[test]
    fn the_later_leg_is_anchored_at_its_distance_from_the_earlier_one() {
        // The mic tap woke 700 ms after the system tap, which is the ordinary
        // case: `system.start()` is called first.
        assert_eq!(
            leg_anchors(Some(5_000_000), Some(705_000_000)),
            LegAnchors {
                system_ms: 0,
                mic_ms: 700
            }
        );
        // And the other way round, because nothing guarantees it.
        assert_eq!(
            leg_anchors(Some(705_000_000), Some(5_000_000)),
            LegAnchors {
                system_ms: 700,
                mic_ms: 0
            }
        );
        // A leg with no t0 has nothing to be lined up against, so neither leg
        // is anchored: an invented offset is worse than none, and this is
        // exactly the behaviour of the whole project before #86.
        assert_eq!(leg_anchors(Some(5_000_000), None), LegAnchors::default());
        assert_eq!(leg_anchors(None, Some(5_000_000)), LegAnchors::default());
        assert_eq!(leg_anchors(None, None), LegAnchors::default());
    }

    /// The payoff, and #86's acceptance test.
    ///
    /// The mic tap here wakes up 700 ms after the system tap, so the audio for
    /// that first 700 ms never reaches the mic's ring at all. That is the
    /// general shape of what #79 fixed one instance of, and the shape #79's
    /// own fix cannot reach: feeding suppressed audio as silence works because
    /// the pump *has* the audio and chooses not to transcribe it. Audio that
    /// was never captured cannot be substituted for.
    ///
    /// Both legs stop at the same instant, so the last sample each socket
    /// swallowed is the same moment in the room. On one session clock the two
    /// must name it as the same millisecond, and the whole of #86 is what makes
    /// that true whatever happened upstream.
    #[test]
    fn a_leg_that_missed_the_start_of_the_meeting_still_agrees_about_the_end() {
        let fed = drive_both_legs_late(true, 700);

        // The provider clocks on their own, which is all that existed before
        // #86: how much PCM each socket swallowed, and nothing else.
        let raw_skew = fed.system_fed_ms().abs_diff(fed.mic_fed_ms());
        assert!(
            raw_skew >= 600,
            "the fixture did not withhold audio from the mic leg — {} ms fed \
             to the system leg against {} ms to the mic — so this proves \
             nothing",
            fed.system_fed_ms(),
            fed.mic_fed_ms()
        );

        // The same two positions, anchored: the mic leg's zero is 700 ms into
        // the session because its first buffer was.
        let anchors = leg_anchors(fed.system_t0_ns, fed.mic_t0_ns);
        let system_end =
            SessionClock::anchored_at(anchors.system_ms).to_session_ms(fed.system_fed_ms());
        let mic_end = SessionClock::anchored_at(anchors.mic_ms).to_session_ms(fed.mic_fed_ms());

        let skew = system_end.abs_diff(mic_end);
        assert!(
            skew <= 50,
            "the legs disagree by {skew} ms about when the meeting ended: \
             system {system_end} ms against mic {mic_end} ms (anchors \
             {anchors:?})"
        );
    }

    /// The control for the test above: two taps that woke together are anchored
    /// at the same place, so anchoring changes nothing about today's meetings.
    #[test]
    fn two_legs_that_started_together_are_both_anchored_at_session_zero() {
        let fed = drive_both_legs(true);

        assert_eq!(
            leg_anchors(fed.system_t0_ns, fed.mic_t0_ns),
            LegAnchors::default()
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
        let counts = pump_loop(
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
            PumpStop {
                stopped: &stop,
                drain: GENEROUS_DRAIN,
            },
        )
        .expect("the pump drains cleanly");
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(
            counts.dropped_samples, expected,
            "the shortfall both rings counted"
        );
    }
}

/// The pump's own way out, and what it leaves behind (#85).
///
/// `pump.join()` was unbounded, and the reason it could hang is in this loop:
/// the stop latch was read only on a pass where both rings came up empty, so a
/// leg that kept delivering after `stop()` kept `moved` true and the pump
/// never looked at the latch again. These drive the real `pump_loop` with
/// prefilled rings — no threads, no timers, byte-identical every run.
#[cfg(test)]
mod pump_drain_tests {
    use super::*;
    use fotw_pipeline::wal::SessionState;
    use std::path::PathBuf;

    const CAPTURE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;

    fn capture_format() -> StreamFormat {
        StreamFormat::new(CAPTURE_RATE, CHANNELS, fotw_audio::SampleFormat::F32)
    }

    fn tmp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("fotwd-drain-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temp root");
        root
    }

    fn no_feeds() -> SttFeeds {
        SttFeeds {
            system: None,
            mic: None,
            echo_gate: None,
        }
    }

    /// A ring the pump cannot empty in one pass, so the deadline decides.
    ///
    /// One short of capacity: the sink drops what does not fit, and a fixture
    /// that measured drops instead of the abandoned tail would say nothing
    /// about the case under test.
    fn full_rings() -> (RingConsumer, RingConsumer, u64) {
        let filling = RING_SAMPLES - 1_024;
        let block = vec![0.25f32; filling];
        let (mut sys_prod, sys_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
        let (mut mic_prod, mic_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
        sys_prod.push_block(&block);
        mic_prod.push_block(&block);
        assert_eq!(
            (sys_cons.dropped_frames(), mic_cons.dropped_frames()),
            (0, 0),
            "the fixture has to fit the ring or the test measures drops instead"
        );
        (sys_cons, mic_cons, filling as u64)
    }

    /// The exit that did not exist: a pump whose rings are still full when its
    /// deadline expires stops anyway.
    #[test]
    fn a_stopped_pump_gives_up_on_a_ring_it_cannot_drain_in_time() {
        let root = tmp_root("abandon");
        let wal = SessionWal::create(&root, CAPTURE_RATE, CHANNELS).expect("a session");
        let dir = wal.dir().to_path_buf();
        let (sys, mic, filled) = full_rings();

        let stop = AtomicBool::new(true);
        let counts = pump_loop(
            wal,
            sys,
            mic,
            capture_format(),
            Some(capture_format()),
            no_feeds(),
            PumpStop {
                stopped: &stop,
                drain: Duration::ZERO,
            },
        )
        .expect("an expired drain is not an error");

        assert!(
            counts.abandoned_samples > 0,
            "the pump reported nothing abandoned, so either it drained a full \
             ring in no time at all or the tail went unreported"
        );
        assert!(
            counts.system_samples < filled,
            "the deadline was ignored: the whole ring was written ({} of {filled})",
            counts.system_samples
        );

        // And the argument for doing it this way rather than abandoning the
        // thread: the session is finalized, so `promote::pending` will take
        // it. An unfinalized one is skipped forever (#79).
        let state = SessionState::read(&dir).expect("the session is readable");
        assert!(
            state.manifest.ended_at_ms.is_some(),
            "an abandoned drain left the session unfinalized, which is the \
             stranded-directory failure this issue exists to avoid"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And the deadline stays invisible to every healthy stop: a ring the pump
    /// has time for is drained to the last sample.
    #[test]
    fn a_stopped_pump_drains_a_ring_it_does_have_time_for() {
        let root = tmp_root("drained");
        let wal = SessionWal::create(&root, CAPTURE_RATE, CHANNELS).expect("a session");
        let (sys, mic, filled) = full_rings();

        let stop = AtomicBool::new(true);
        let counts = pump_loop(
            wal,
            sys,
            mic,
            capture_format(),
            Some(capture_format()),
            no_feeds(),
            PumpStop {
                stopped: &stop,
                drain: GENEROUS_DRAIN,
            },
        )
        .expect("the pump drains cleanly");

        assert_eq!(
            (counts.system_samples, counts.mic_samples),
            (filled, filled),
            "the drain deadline ate audio a healthy stop had time to write"
        );
        assert_eq!(
            counts.abandoned_samples, 0,
            "a clean drain must not report an abandoned tail"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

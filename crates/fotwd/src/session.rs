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
    /// What the transcription provider failed with, if anything.
    ///
    /// Empty is not the same as "transcription worked": a session with
    /// [`Transcription::Disabled`] also has none. The two are distinguished by
    /// what the caller configured, not by this field.
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
type SegmentFn = dyn Fn(&TranscriptSegment) + Send + Sync;

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
    pub fn new(f: impl Fn(&TranscriptSegment) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Hand one segment over, or do nothing when no tap was set.
    pub fn emit(&self, segment: &TranscriptSegment) {
        if let Some(f) = &self.0 {
            f(segment);
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
        system: sys_stt.clone(),
        mic: mic_stt.clone(),
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
                    tap.emit(&seg);
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(seg);
                }
                StreamEvent::Error(e) => {
                    eprintln!("  ! transcription ({leg}): {e}");
                    errors.record(format!("{leg}: {e}"));
                }
                _ => {}
            }
        }
    })
}

/// The provider connections the pump feeds, one per leg.
///
/// A struct rather than two more parameters: the pump's argument list was at
/// clippy's limit, and these two travel together or not at all.
struct SttFeeds {
    system: Option<Arc<DeepgramStream>>,
    mic: Option<Arc<DeepgramStream>>,
}

/// Drain both rings until stopped, writing raw audio and feeding the provider.
fn pump_loop(
    mut wal: SessionWal,
    mut sys: RingConsumer,
    mut mic: RingConsumer,
    sys_format: StreamFormat,
    mic_format: Option<StreamFormat>,
    stt: SttFeeds,
    stop: &AtomicBool,
) -> Result<(u64, u64, u64), String> {
    let mut scratch = vec![0.0f32; 48_000];
    let (mut sys_written, mut mic_written) = (0u64, 0u64);

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
                    h.write(&Downmixer::to_i16(&mono));
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
                    h.write(&Downmixer::to_i16(&mono));
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

    wal.flush().map_err(|e| format!("flush failed: {e}"))?;
    // Finalize stamps `ended_at_ms`. Without it every cleanly-ended meeting
    // looks crashed and reappears in the recovery list forever, which trains
    // the user to ignore the one prompt that matters.
    wal.finalize()
        .map_err(|e| format!("finalize failed: {e}"))?;
    Ok((sys_written, mic_written, 0))
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

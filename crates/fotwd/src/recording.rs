//! The daemon half of the UI's Start button.
//!
//! [`fotw_web::RecorderControl`] is the seam; this is the implementation that
//! knows about devices, the keychain and the library. It is the same sequence
//! `fotwd record` performs, with the printing removed and the stopwatch
//! replaced by a [`StopSignal`].
//!
//! # The TCC trap this module cannot fix on its own
//!
//! macOS attributes a system-audio grant to the *responsible process*, which
//! for a daemon started from a shell is the terminal, not us. A `fotwd serve`
//! launched from Ghostty therefore records through the terminal's grant — and
//! a developer's machine reports success where a user's machine yields
//! silence. The Start button is only correct when the daemon was launched as
//! the bundle:
//!
//! ```text
//! open -a /path/to/FlyOnTheWall.app --args serve --port 8765
//! ```
//!
//! [`DaemonRecorder::launched_as_app`] reports which of the two happened, so
//! the UI can say so rather than producing a silent recording.
//!
//! # The finishing window, and why it is short
//!
//! `stop()` trips the signal and returns immediately: an HTTP handler that
//! waited for the wind-down would hang. The slot then stays occupied for as
//! long as the wind-down takes — draining the ring and closing the taps — and
//! a second Start during that window is refused rather than opening a second
//! tap on the same device.
//!
//! What that window is *called* was the bug. It used to read `recording`,
//! which is true of the slot and false of everything the user can see: the tap
//! is shut, the meeting cannot get any longer, and the dashboard drew a
//! climbing clock over a microphone that was already closed. Status now reads
//! `finishing` there, with the clock frozen at the length the meeting ended on
//! (#77). The guard is unchanged — `Finishing` is not `Idle`.
//!
//! # Why the slot frees before persist
//!
//! The window closes the moment capture is safe on disk — before the meeting
//! is encoded to Opus and written to the library, not after. `run_…` does not
//! return until the WAL is written and the taps are closed-or-abandoned, both
//! bounded by [`FinishDeadlines`] (#85), so by the time the session task can
//! run its tail the recorder owes nothing to a device or a buffer. It frees
//! the slot there, and persist-and-promote plus [`enrich_and_announce`] run
//! afterward as pure background work over a file that is already safe. That is
//! what lets the *next* meeting start while this one finalizes: persist alone
//! is minutes on a long call, and holding the slot across it meant a user who
//! stopped one meeting and reached for the next was refused for those minutes.
//!
//! That the wind-down ends *within a bounded time* is what makes freeing there
//! safe, and it did not until #85. Every blocking step — closing the taps,
//! waiting for the pump — now runs under [`FinishDeadlines`], because none of
//! them could be cancelled once entered: a Core Audio HAL blocks in `stop()`
//! for the same reason it blocks in `start()`, and a `pump.join()` with no
//! clock turned that into a slot nobody could free.
//!
//! That is a real concurrency change and this header owns it: persist and
//! enrichment can now overlap live capture of the next meeting, and one
//! meeting's persist or enrichment can overlap another's. It is safe for the
//! library — SQLite runs in WAL with a busy timeout (`fotw-store/src/db.rs`),
//! each meeting writes its own session directory and rows, and retention keys
//! off session-dir mtime rather than the slot — but it is real CPU beside live
//! Opus encoding and streaming transcription.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fotw_audio::clock::{Clock, HostClock};
use fotw_audio::{AudioPlatform, AudioTap, platform};
use fotw_secrets::KeyStore;
use fotw_shell::StartOrigin;

use fotw_web::{MeetingReadyReason, RecorderControl, RecorderError, RecordingStatus};

use crate::audit::{AuditKind, AuditLog};
use crate::consent::{JurisdictionSignals, Rules};
use crate::secrets;
use crate::session::{
    self, DeepgramLegs, FinishDeadlines, SegmentTap, SessionControl, SessionOutcome, StopSignal,
    SttErrors, Transcription,
};
use crate::stt::{SttEngine, SttSettings};
// This module runs entirely inside the daemon, where stderr is discarded, so
// every diagnostic below is also written to the daemon's log (#101). The two
// that handle transcript-derived text deliberately do NOT use `diag!` — see
// `enrich_and_announce`.
use crate::{diag, journal};

/// How a session acquires its taps.
///
/// A function rather than a hardcoded call to [`platform::host`] so the state
/// machine can be exercised on a CI runner with no audio device, using the
/// same `FileAudioSource` the session tests use.
pub type TapOpener =
    Box<dyn Fn() -> Result<(Box<dyn AudioTap>, Option<Box<dyn AudioTap>>), String> + Send + Sync>;

/// How this recorder captures audio.
///
/// The daemon and its tests take different paths through the *same* session
/// machinery. Production ([`DaemonRecorder::new`]) supervises taps it opens
/// itself, so a Core Audio process tap that stalls mid-meeting is rebuilt in
/// place rather than recording silence to the end (docs/REQUIREMENTS.md 6.4,
/// #101) — the recovery the CLI (`fotw::record`) has always had and the daemon
/// never wired. The tests ([`DaemonRecorder::with_parts`]) hand the session
/// pre-started taps over a `FileAudioSource`, which never stalls and needs no
/// supervisor; keeping that path is what lets the daemon's state machine run on
/// a CI box with no audio device.
enum CaptureMode {
    /// Taps opened once, up front, and handed to the session pre-started. No
    /// live rebuild.
    Prebuilt(Arc<TapOpener>),
    /// The backend and clock the session builds a [`fotw_audio::supervisor::CaptureSupervisor`]
    /// per leg from, rebuilding a stalled tap for the life of the meeting.
    Supervised {
        plat: Arc<dyn AudioPlatform>,
        clock: Arc<dyn Clock>,
    },
}

/// One session's capture payload, moved onto the session task. Both paths open
/// taps only after transcription admission. Startup failures wake the waiting
/// caller; a wedged native operation remains bounded by the ready deadline.
enum SessionCapture {
    Prebuilt(Arc<TapOpener>),
    Supervised {
        plat: Arc<dyn AudioPlatform>,
        clock: Arc<dyn Clock>,
    },
}

/// What to do with a finished session. Injectable for the same reason.
///
/// Returns the persisted meeting id when there is one, so the caller can run
/// enrichment (#67/#68) against it — titles and summaries need a row to hang
/// off, and only the finisher knows whether persist succeeded.
pub type Finisher = Box<dyn Fn(&Path, &SessionOutcome) -> Option<String> + Send + Sync>;

/// How a session decides whether, and how, to transcribe.
///
/// Injectable because the real one reads the OS keychain, and a test that
/// reads the *user's* keychain is not merely impure — it raises an approval
/// dialog. `cargo` re-signs a test binary ad-hoc on every build, so each run
/// presents a new code identity to an item whose ACL is bound to the identity
/// that created it, and macOS prompts every single time.
pub type TranscriptionFactory = Box<dyn Fn() -> Transcription + Send + Sync>;

/// The closure shape a [`ReadyTap`] holds.
type ReadyFn = dyn Fn(&str, MeetingReadyReason, Option<&str>) + Send + Sync;

/// A callback told when a meeting becomes worth fetching (#78).
///
/// The library-changed seam, built the same way as [`SegmentTap`] and for the
/// same reason: the hub lives inside the web server's state, which does not
/// exist until after the recorder does, so the recorder takes a callback
/// rather than a hub. `serve` hands over one that announces on the hub; the
/// default is silence, so every caller that does not name the feature — the
/// tests, and any future embedder — keeps its old behaviour.
///
/// It must not block: it fires from the `spawn_blocking` thread the finisher
/// runs on, between persist and promote, and anything that waited there would
/// hold the recorder's slot. [`fotw_web::DeltaHub::announce_meeting_ready`] is
/// a synchronous `broadcast::send` by design.
///
/// # What this deliberately does not reach
///
/// `fotwd record` is a **separate process** from `fotwd serve`. It persists
/// through its own [`crate::open_library`] handle (`main.rs`) and can never
/// reach the serving process's in-memory hub without IPC, which #78 did not
/// take on. A meeting recorded from the CLI while a dashboard is open is
/// therefore still reload-only. Said here rather than discovered.
#[derive(Clone, Default)]
pub struct ReadyTap(Option<Arc<ReadyFn>>);

impl std::fmt::Debug for ReadyTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The meeting id is not content, but it names a row in the user's
        // library, and §10's never-log rule does not stop at log files.
        f.write_str(if self.0.is_some() {
            "ReadyTap(<set>)"
        } else {
            "ReadyTap(<none>)"
        })
    }
}

impl ReadyTap {
    /// A tap that hands each announcement to `f`.
    #[must_use]
    pub fn new(f: impl Fn(&str, MeetingReadyReason) + Send + Sync + 'static) -> Self {
        Self::with_session(move |id, reason, _session| f(id, reason))
    }

    /// Subscribe with the capture identity, for navigation-safe UI handoff.
    #[must_use]
    pub fn with_session(
        f: impl Fn(&str, MeetingReadyReason, Option<&str>) + Send + Sync + 'static,
    ) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Announce one meeting, or do nothing when no tap was set.
    pub fn emit(&self, meeting_id: &str, reason: MeetingReadyReason) {
        self.emit_session(meeting_id, reason, None);
    }

    /// Announce the saved meeting together with its source capture identity.
    pub fn emit_session(
        &self,
        meeting_id: &str,
        reason: MeetingReadyReason,
        session_id: Option<&str>,
    ) {
        if let Some(f) = &self.0 {
            f(meeting_id, reason, session_id);
        }
    }
}

/// A recording in flight, or one still writing itself to disk.
struct Live {
    speech: crate::speech_status::SpeechMonitor,
    capture_health: crate::capture_health::CaptureMonitor,
    ready: crate::session::ReadySignal,
    stop: StopSignal,
    started_at_ms: u64,
    /// When Stop was pressed, once it has been. `Some` is what separates
    /// finishing from recording, and it is the frozen clock's only source.
    ///
    /// A plain field rather than an atomic: every read and write of it already
    /// holds the mutex around the slot, so an atomic would buy nothing and
    /// invite a caller to touch it without one.
    stopped_at_ms: Option<u64>,
    /// So `status()` can report a provider failure while the meeting is still
    /// running, rather than leaving it to be discovered in an empty file.
    errors: SttErrors,
}

/// The recorder state and its exact stop target, observed for native controls.
#[derive(Clone, Debug)]
pub struct RecorderObservation {
    /// Authoritative capture/preparation state.
    status: RecordingStatus,
    /// An opaque target for this attempt, including pre-capture preparation.
    stop_target: Option<StopTarget>,
}

impl RecorderObservation {
    /// The status captured with this observation's stop capability.
    #[must_use]
    pub const fn status(&self) -> &RecordingStatus {
        &self.status
    }

    /// The matching capability; absent when no attempt occupied the slot.
    #[must_use]
    pub const fn stop_target(&self) -> Option<&StopTarget> {
        self.stop_target.as_ref()
    }
}

/// An in-process capability to stop one observed attempt, never its successor.
#[derive(Clone)]
pub struct StopTarget(crate::session::ReadySignal);

impl StopTarget {
    /// Whether two observations name exactly the same in-process attempt.
    #[must_use]
    pub fn same_attempt(&self, other: &Self) -> bool {
        self.0.same_session(&other.0)
    }
}

type VisibilityFn = dyn Fn(RecorderObservation, StopSignal) -> Result<(), String> + Send + Sync;

/// A native renderer's admission check, run off the recorder lock before setup.
#[derive(Clone)]
pub struct StartVisibility(Arc<VisibilityFn>);

impl StartVisibility {
    /// Construct a bounded admission check. It must honor cancellation and refuse
    /// capture when the native recording indicator cannot be rendered.
    pub fn new(
        check: impl Fn(RecorderObservation, StopSignal) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(check))
    }
}

impl std::fmt::Debug for StopTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StopTarget(<redacted>)")
    }
}

/// Starts and stops real recordings on the UI's behalf.
pub struct DaemonRecorder {
    root: PathBuf,
    handle: tokio::runtime::Handle,
    capture: CaptureMode,
    transcription: TranscriptionFactory,
    finish: Arc<Finisher>,
    ceiling: Duration,
    ready_deadline: Duration,
    /// How long a session's wind-down may take before it stops waiting on the
    /// device — where `Finishing` ends (#85).
    finish_deadlines: FinishDeadlines,
    /// Handed to every session for the live transcript (#61).
    on_segment: SegmentTap,
    /// Told when a meeting lands in the library, so an open tab can refetch
    /// without being reloaded (#78).
    on_ready: ReadyTap,
    live: Arc<Mutex<Option<Live>>>,
    start_visibility: Option<StartVisibility>,
    lifecycle: Arc<crate::recorder_lifecycle::Lifecycle>,
}

impl std::fmt::Debug for DaemonRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the root: it names the directory the meetings are in.
        f.write_str("DaemonRecorder(<redacted>)")
    }
}

/// The ceiling a UI-started recording gets if nobody stops it.
///
/// Not unbounded: `retention::recording_in_flight` vetoes the sweeper while a
/// session is alive, so a browser tab closed and forgotten would disable
/// retention indefinitely as well as fill the disk. Two hours bounds an
/// unattended meeting; the session also checks wall time across laptop sleep.
pub const UI_CEILING: Duration = Duration::from_secs(2 * 60 * 60);

/// How long `start()` waits for capture to actually begin before calling it a
/// failure.
///
/// Generous, because opening a real device is not instant, but finite: a Core
/// Audio device whose HAL still believes a dead client holds it blocks in
/// `start()` forever rather than returning an error, and nothing inside this
/// process can cancel the syscall it is stuck in. Fifteen seconds is longer
/// than any healthy start and short enough that a user learns the truth while
/// they are still looking at the button.
pub const READY_DEADLINE: Duration = Duration::from_secs(15);

impl DaemonRecorder {
    /// Atomic-only admission capability for the native thread. Device Stop
    /// still runs on the bridge worker; no recorder mutex is taken here.
    pub(crate) fn admission_closer(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.lifecycle.closing)
    }

    /// Permanently close Start admission and request Stop on the current attempt.
    /// Call on a background worker; this never waits for capture/persistence.
    pub fn begin_shutdown(&self) {
        self.lifecycle
            .closing
            .store(true, std::sync::atomic::Ordering::Release);
        let mut slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(live) = slot.as_mut() {
            stop_live(live);
        }
    }

    /// Observe accepted work, including persistence after the device slot frees.
    #[must_use]
    pub fn shutdown_status(&self) -> crate::recorder_lifecycle::ShutdownStatus {
        let slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        self.lifecycle.status(slot.is_some())
    }

    /// Observe state for the native indicator without opening any capture.
    #[must_use]
    pub fn observe(&self) -> RecorderObservation {
        let slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map_or_else(
            || RecorderObservation {
                status: RecordingStatus::idle(),
                stop_target: None,
            },
            |live| RecorderObservation {
                status: live_status(live, self.ceiling),
                stop_target: Some(StopTarget(live.ready.clone())),
            },
        )
    }

    /// Stop the exact attempt represented by an earlier native observation.
    /// Validation and Stop hold the same slot lock, so replacement cannot race
    /// between checking the target and issuing the command.
    ///
    /// # Errors
    /// If the attempt ended or was replaced, without changing a successor.
    pub fn stop_observed(&self, target: &StopTarget) -> Result<RecordingStatus, RecorderError> {
        let mut slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let live = slot.as_mut().ok_or(RecorderError::NotRecording)?;
        if !live.ready.same_session(&target.0) {
            return Err(RecorderError::Failed(
                "recording changed; refresh its status".into(),
            ));
        }
        Ok(stop_live(live))
    }

    /// Start requested by the native shell after its explicit consent action.
    ///
    /// # Errors
    /// When acknowledgement is absent, the request is cancelled, or capture fails.
    pub fn start_native(
        &self,
        origin: StartOrigin,
        acknowledged: bool,
        cancel: StopSignal,
    ) -> Result<RecordingStatus, RecorderError> {
        if !acknowledged {
            return Err(RecorderError::Failed(
                "confirm that everyone consented before recording".into(),
            ));
        }
        self.start_admitted(origin, cancel)
    }

    fn start_admitted(
        &self,
        origin: StartOrigin,
        cancel: StopSignal,
    ) -> Result<RecordingStatus, RecorderError> {
        let mut slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .lifecycle
            .closing
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(RecorderError::Failed(
                "the app is shutting down; recording was not started".into(),
            ));
        }
        if cancel.is_stopped() {
            return Err(RecorderError::Failed(
                "recording start cancelled before capture".into(),
            ));
        }
        if slot.is_some() {
            return Err(RecorderError::AlreadyRecording);
        }

        let work = self.lifecycle.begin();
        let mut control = SessionControl {
            stop: cancel,
            ..SessionControl::new()
        };
        control.on_segment = self.on_segment.clone();
        control.deadlines = self.finish_deadlines;
        let stop = control.stop.clone();
        let ready = control.ready.clone();
        ready.set_preparing(true);
        let started_at_ms = now_ms();
        let capture_health = control.capture_health.clone();
        *slot = Some(Live {
            speech: control.speech.clone(),
            capture_health: capture_health.clone(),
            ready: ready.clone(),
            stop: stop.clone(),
            started_at_ms,
            stopped_at_ms: None,
            errors: control.errors.clone(),
        });
        drop(slot);

        // Configuration and audit I/O must not own the live-slot mutex. The
        // reserved attempt stays observable/cancellable and rejects a second
        // Start until this owner either dispatches capture or releases it.
        let mut pending = PendingStart {
            live: Arc::clone(&self.live),
            ready: ready.clone(),
            stop: stop.clone(),
            armed: true,
            work: Some(work),
        };

        // CON-05: the gate runs before anything is captured. The HTTP layer
        // or native admission has already refused a start with no acknowledgement,
        // so reaching here means a human ticked the box; what is recorded here is *what
        // they were warned about*.
        let home = std::env::var("FOTW_JURISDICTION").unwrap_or_else(|_| "US-CA".to_owned());
        let escalation = Rules::builtin().escalate(&JurisdictionSignals::home(&home));

        // CON-01's acceptance criterion is about the audit log, not the UI: no
        // audio buffer reaches disk without a user-initiated Start recorded
        // first. Written *before* the tap opens, so a crash during capture
        // still leaves the record of who asked.
        let audit = AuditLog::at(&self.root);
        audit
            .record(AuditKind::SessionStart {
                origin: origin.label().to_owned(),
                detected_app: None,
                jurisdiction_warning: escalation.user_text(),
                acknowledged_all_party: escalation.blocks(),
            })
            .map_err(|e| {
                // A recording we cannot account for is not one we should make.
                RecorderError::Failed(format!("could not write the audit log: {e}"))
            })?;

        if let Some(visibility) = &self.start_visibility {
            (visibility.0)(self.observe(), stop.clone()).map_err(RecorderError::Failed)?;
        }

        // Defer all tap opening until the session owns its slot and finishes
        // transcription admission. Opening a tap may itself access a device.
        let capture = match &self.capture {
            CaptureMode::Prebuilt(open_taps) => SessionCapture::Prebuilt(Arc::clone(open_taps)),
            CaptureMode::Supervised { plat, clock } => SessionCapture::Supervised {
                plat: Arc::clone(plat),
                clock: Arc::clone(clock),
            },
        };

        if stop.is_stopped() {
            return Err(RecorderError::Failed(
                "recording start cancelled before configuration".into(),
            ));
        }
        let transcription = (self.transcription)();
        if stop.is_stopped() {
            return Err(RecorderError::Failed(
                "recording start cancelled before capture".into(),
            ));
        }
        ready.set_preparing(transcription.needs_preparation());

        let root = self.root.clone();
        let live = Arc::clone(&self.live);
        let ceiling = self.ceiling;
        // Behind an `Arc` so the task owns a handle without the recorder
        // having to be `Clone`.
        let finish = Arc::clone(&self.finish);

        self.handle.spawn(spawn_session(
            root,
            capture,
            transcription,
            ceiling,
            control,
            started_at_ms,
            Arc::clone(&live),
            finish,
            self.on_ready.clone(),
            pending.work.take().expect("accepted work owner"),
        ));
        pending.armed = false;

        // Do not answer until capture is real. Answering on spawn is what let
        // a wedged device show a RECORDING badge over an empty disk.
        if !ready.wait_timeout(self.ready_deadline) {
            if let Some(error) = ready.failure() {
                return Err(RecorderError::Failed(error));
            }
            let preparing = ready.is_preparing();
            capture_health.close();
            ready.request_stop(&stop);
            let error = if preparing {
                "local backup model preparation timed out; capture was not started. Waiting for native preparation to release its resources."
            } else {
                "recording setup timed out; waiting for native setup to release its resources before another Start. A blocked Core Audio HAL may require restarting coreaudiod and the app."
            }.to_owned();
            // Deadlines bound the caller, not an OS operation. Every mode keeps
            // its native owner until spawn_session actually completes cleanup.
            let slot = live.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(owner) = slot.as_ref().filter(|s| s.ready.same_session(&ready)) {
                owner.errors.record(error.clone());
            }
            ready.fail(error.clone());
            return Err(RecorderError::Failed(error));
        }

        Ok(self.status())
    }

    /// A recorder that opens the host's real devices.
    #[must_use]
    pub fn new(
        root: PathBuf,
        handle: tokio::runtime::Handle,
        on_segment: SegmentTap,
        on_ready: ReadyTap,
    ) -> Self {
        // The finisher is where `persisted` is announced, so the real one
        // carries the tap with it. `with_parts` takes the finisher whole, so a
        // test that injects its own gets no announcements — which is right:
        // there is no hub behind them.
        let finish_ready = on_ready.clone();
        let transcription_root = root.clone();
        // The real daemon supervises the taps it opens itself, so a stalled
        // Core Audio process tap is rebuilt mid-meeting instead of recording
        // silence to the end (docs/REQUIREMENTS.md 6.4, #101). The session
        // builds the per-leg supervisors from this backend and clock, opening
        // the taps inside its own task; a machine with no input device still
        // records the far end because the mic leg is optional there too.
        Self::assemble(
            root,
            handle,
            on_segment,
            on_ready,
            CaptureMode::Supervised {
                plat: Arc::new(platform::host()),
                clock: Arc::new(HostClock),
            },
            Box::new(move || configured_transcription(&transcription_root)),
            Box::new(move |root, outcome| persist_and_promote(root, outcome, &finish_ready)),
            UI_CEILING,
            READY_DEADLINE,
        )
    }

    /// [`DaemonRecorder::new`] with every dependency named, for tests.
    ///
    /// The taps are handed in pre-startable rather than supervised: a
    /// `FileAudioSource` never stalls, so there is nothing to rebuild, and the
    /// daemon's state machine can run on a CI box with no audio device.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // every dependency named, as the tests require
    pub fn with_parts(
        root: PathBuf,
        handle: tokio::runtime::Handle,
        on_segment: SegmentTap,
        on_ready: ReadyTap,
        open_taps: TapOpener,
        transcription: TranscriptionFactory,
        finish: Finisher,
        ceiling: Duration,
        ready_deadline: Duration,
    ) -> Self {
        Self::assemble(
            root,
            handle,
            on_segment,
            on_ready,
            CaptureMode::Prebuilt(Arc::new(open_taps)),
            transcription,
            finish,
            ceiling,
            ready_deadline,
        )
    }

    /// The shared body of [`new`](Self::new) and [`with_parts`](Self::with_parts):
    /// they differ only in how audio is captured, so the one field that varies
    /// is passed in and everything else is built the same way.
    #[allow(clippy::too_many_arguments)] // the union of both public constructors
    fn assemble(
        root: PathBuf,
        handle: tokio::runtime::Handle,
        on_segment: SegmentTap,
        on_ready: ReadyTap,
        capture: CaptureMode,
        transcription: TranscriptionFactory,
        finish: Finisher,
        ceiling: Duration,
        ready_deadline: Duration,
    ) -> Self {
        Self {
            root,
            handle,
            capture,
            transcription,
            finish: Arc::new(finish),
            ceiling,
            ready_deadline,
            finish_deadlines: FinishDeadlines::default(),
            on_segment,
            on_ready,
            live: Arc::new(Mutex::new(None)),
            start_visibility: None,
            lifecycle: Arc::default(),
        }
    }

    /// The same recorder, finishing on a different clock (#85).
    ///
    /// A chained setter rather than a tenth argument to
    /// [`with_parts`](Self::with_parts): the shipped defaults are the whole
    /// story for the daemon, and the only caller that needs anything else is a
    /// test that would otherwise sit out a ten-second drain to watch the slot
    /// free.
    #[must_use]
    pub const fn with_finish_deadlines(mut self, deadlines: FinishDeadlines) -> Self {
        self.finish_deadlines = deadlines;
        self
    }

    /// Require native visibility before any transcription configuration or tap
    /// opening. Install before sharing the recorder with HTTP or shell controls.
    #[must_use]
    pub fn with_start_visibility(mut self, visibility: StartVisibility) -> Self {
        self.start_visibility = Some(visibility);
        self
    }

    /// Whether this process was launched as the app bundle rather than from a
    /// shell — which is what decides who owns the audio grant.
    ///
    /// Reads the executable path rather than an environment variable, because
    /// the variable a terminal sets is exactly what a user's shell profile
    /// might also set.
    #[must_use]
    pub fn launched_as_app() -> bool {
        std::env::current_exe().is_ok_and(|p| {
            p.components()
                .any(|c| c.as_os_str().to_string_lossy().ends_with(".app"))
        })
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What `Start` transcribes with: the engine `fotwd stt` stored (#33).
///
/// Read at every Start, so a change applies to the next meeting without a
/// restart. Fails closed: a library that cannot be read leaves the choice
/// unknown, and the unknown choice may be "audio never leaves this machine" —
/// so the meeting records untranscribed rather than guessing Deepgram.
fn configured_transcription(root: &Path) -> Transcription {
    transcription_for_choice(
        crate::open_library(root).and_then(|db| SttSettings::read(&db)),
        root,
        keychain_transcription,
    )
}

fn transcription_for_choice(
    choice: Result<SttSettings, String>,
    root: &Path,
    deepgram: impl FnOnce() -> Transcription + Send + 'static,
) -> Transcription {
    let settings = match choice {
        Ok(settings) => settings,
        Err(e) => {
            diag!(
                "  ! could not read the transcription settings; recording without a transcript: {e}"
            );
            return Transcription::Disabled;
        }
    };
    match settings.engine {
        SttEngine::Deepgram => deepgram(),
        SttEngine::Fallback => crate::stt::fallback_transcription(
            settings,
            root.parent().unwrap_or(root).to_owned(),
            Box::new(move || {
                match deepgram() {
                Transcription::Deepgram(legs) => Ok(legs),
                _ => Err("Deepgram-to-local mode requires a Deepgram key; add one or explicitly choose local-only".into()),
            }
            }),
        ),
        SttEngine::Local => crate::stt::local_transcription(
            &settings,
            root.parent().unwrap_or(root),
            &format!("fotw-web-{}", now_ms()),
            // As for Deepgram: whether a mic started is the session's call.
            true,
            session::mic_stt_enabled(std::env::var("FOTW_MIC_STT").ok().as_deref()),
        ),
    }
}

#[cfg(test)]
mod transcription_choice_tests {
    use super::*;

    #[test]
    fn invalid_saved_settings_never_reach_the_cloud_factory() {
        let mut db =
            fotw_store::Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x36; 32])).unwrap();
        for saved in ["{", "{}", r#"{"engine":"unknown-local"}"#] {
            db.put_setting(crate::stt::SETTINGS_KEY, saved).unwrap();
            let choice = SttSettings::read(&db);
            assert!(choice.is_err());
            let transcription =
                transcription_for_choice(choice, Path::new("/unused-fixture"), || {
                    panic!("invalid settings must not look up a cloud key or open a provider")
                });
            assert!(matches!(transcription, Transcription::Disabled));
        }
    }

    #[test]
    fn a_settings_read_failure_never_reaches_the_cloud_factory() {
        let db =
            fotw_store::Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x37; 32])).unwrap();
        db.conn().execute_batch("DROP TABLE settings").unwrap();
        let choice = SttSettings::read(&db);
        assert!(choice.is_err());
        assert!(matches!(
            transcription_for_choice(choice, Path::new("/unused-fixture"), || {
                panic!("failed settings read must not select cloud")
            }),
            Transcription::Disabled
        ));
    }
}

/// Look the Deepgram key up in the OS keychain, with `DEEPGRAM_API_KEY` as the
/// fallback, as `fotwd record` does.
///
/// Both legs are transcribed unless `FOTW_MIC_STT` turns the mic leg off
/// ([`session::mic_stt_enabled`]; unset means on). The mic leg needs its own
/// Deepgram connection and doubles the bill (#60), and it is opened only if
/// the session's mic actually starts.
fn keychain_transcription() -> Transcription {
    let store = secrets::keystore().ok();
    match store
        .as_ref()
        .and_then(|s| secrets::deepgram_key(*s as &dyn KeyStore))
    {
        Some((secret, _)) => Transcription::Deepgram(DeepgramLegs::for_session(
            secret.expose(),
            &format!("fotw-web-{}", now_ms()),
            // Whether a mic actually started is the session's call, not ours:
            // it gates the paid mic stream on the tap coming up, so claiming
            // one here costs nothing when the device is absent.
            true,
            session::mic_stt_enabled(std::env::var("FOTW_MIC_STT").ok().as_deref()),
        )),
        None => Transcription::Disabled,
    }
}

/// The persist-then-promote tail, headless.
///
/// Persist happens AFTER the WAL is finalized, never instead of it: the
/// database is an index over the session directory, so if this fails the
/// meeting is still on disk and can be imported again. Promotion then moves it
/// out of the scratch `sessions/` lifetime into the durable one — skipping
/// that is what left every recording sitting in `sessions/` with the retention
/// engine unable to see any of it.
///
/// `ready` is told **between** the two, not after (#78). `persist_session`
/// leaves the row in `ready` and queryable, so the dashboard can list the
/// meeting from that instant; promotion then re-encodes the whole call to
/// Opus, which is minutes on a long one. Announcing after promotion would
/// leave the library stale for exactly as long as the encode takes, and would
/// announce nothing at all when promotion fails — even though the row it names
/// is right there.
fn persist_and_promote(root: &Path, outcome: &SessionOutcome, ready: &ReadyTap) -> Option<String> {
    if !outcome.segments.is_empty()
        && let Err(e) = session::append_segments(&outcome.dir, &outcome.segments)
    {
        diag!("  ! could not append the transcript: {e}");
    }

    let data_root = root.parent().unwrap_or(root).to_path_buf();
    // A dated placeholder, not an epoch second: enrichment replaces it within
    // seconds when there is speech to name, and a meeting recorded in silence
    // keeps this forever (#76, and #67's transcript-less acceptance).
    //
    // The date it carries is UTC and says so, which reads as wrong to anyone
    // not on it. That is a constraint, not an oversight: the workspace has no
    // timezone database at all, so UTC is the only clock it can name honestly,
    // and rendering true local time is a dependency decision. Read
    // `dated_fallback_title`'s doc before reaching for a conversion here (#89).
    let title = crate::enrich::dated_fallback_title(fotw_store::now_ms());

    match crate::open_library(root) {
        Err(e) => {
            diag!("  ! could not open the library: {e}");
            None
        }
        Ok(mut db) => match crate::persist::persist_session(&mut db, outcome, &title) {
            Err(e) => {
                diag!("  ! could not add to the library: {e}");
                None
            }
            Ok(id) => {
                // The id and nothing else: it is what `fotwd summarize <id>`
                // and every follow-up question take, and it is the only part
                // of a meeting §10 lets a durable file hold.
                journal::record(&format!("recording: meeting {id} persisted"));
                // Before promotion, on purpose — see this function's header.
                ready.emit_session(
                    &id,
                    MeetingReadyReason::Persisted,
                    outcome.dir.file_name().and_then(|s| s.to_str()),
                );
                if let Err(e) = crate::retention::promote_session(
                    &mut db,
                    &data_root,
                    &outcome.dir,
                    &id,
                    outcome.started_at_ms,
                ) {
                    diag!("  ! could not archive the session: {e}");
                }
                Some(id)
            }
        },
    }
}

impl RecorderControl for DaemonRecorder {
    fn start(&self) -> Result<RecordingStatus, RecorderError> {
        self.start_admitted(StartOrigin::WebUi, StopSignal::new())
    }

    fn stop(&self) -> Result<RecordingStatus, RecorderError> {
        let mut slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let live = slot.as_mut().ok_or(RecorderError::NotRecording)?;
        Ok(stop_live(live))
    }

    fn retry_capture(
        &self,
        session_id: &str,
        source: fotw_web::recorder::CaptureSource,
    ) -> Result<(), RecorderError> {
        let slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = slot.as_ref() else {
            return Err(RecorderError::NotRecording);
        };
        if live.stop.is_stopped()
            || live.stopped_at_ms.is_some()
            || live.ready.capture_ended_at_ms().is_some()
            || live.ready.session_id().as_deref() != Some(session_id)
        {
            return Err(RecorderError::Failed(
                "recording changed; refresh its status".into(),
            ));
        }
        if !live.capture_health.request_retry(source) {
            return Err(RecorderError::Failed(
                "this source is not waiting for a manual retry".into(),
            ));
        }
        Ok(())
    }

    fn status(&self) -> RecordingStatus {
        self.observe().status
    }
}

/// Releases only this reservation if pre-dispatch I/O fails or unwinds.
/// Once dispatched, the session task owns cleanup, including native retention.
struct PendingStart {
    live: Arc<Mutex<Option<Live>>>,
    ready: crate::session::ReadySignal,
    stop: StopSignal,
    armed: bool,
    work: Option<crate::recorder_lifecycle::WorkLease>,
}

impl Drop for PendingStart {
    fn drop(&mut self) {
        if self.armed {
            self.ready.request_stop(&self.stop);
            self.ready
                .fail("recording preparation ended before capture".into());
            clear_session_slot(&self.live, &self.ready);
        }
    }
}

fn clear_session_slot(live: &Arc<Mutex<Option<Live>>>, ready: &crate::session::ReadySignal) {
    let mut slot = live.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|s| s.ready.same_session(ready)) {
        *slot = None;
    }
}

/// Persist/enrich independently of retained native work. Release the slot
/// when measured native-worker ownership is clear.
#[allow(clippy::too_many_arguments)]
async fn spawn_session(
    root: PathBuf,
    capture: SessionCapture,
    transcription: Transcription,
    ceiling: Duration,
    control: SessionControl,
    started_at_ms: u64,
    live: Arc<Mutex<Option<Live>>>,
    finish: Arc<Finisher>,
    on_ready: ReadyTap,
    mut work: crate::recorder_lifecycle::WorkLease,
) {
    // Prebuilt taps run the session that hands them over pre-started; the
    // supervised backend runs the one that builds a per-leg supervisor and
    // rebuilds a stalled tap in place (#101). The finishing tail below is
    // identical, so only the capture call differs.
    let capture_health = control.capture_health.clone();
    let speech = control.speech.clone();
    let ready = control.ready.clone();
    let outcome = async {
        if control.stop.is_stopped() {
            return Err("recording preparation cancelled before capture".into());
        }
        let model_work = Arc::new(crate::model_setup::WorkLease::capture(&root)?);
        let transcription = transcription.prepare(&control).await?;
        if !control.ready.begin_capture_setup(&control.stop) {
            return Err("recording preparation cancelled before capture".into());
        }
        match capture {
            SessionCapture::Prebuilt(open) => {
                let open_work = model_work.clone();
                let (system, mic) = tokio::task::spawn_blocking(move || {
                    let (system, mic) = open()?;
                    Ok::<_, String>((
                        crate::model_setup::lease_tap(system, open_work.clone()),
                        mic.map(|tap| crate::model_setup::lease_tap(tap, open_work.clone())),
                    ))
                })
                .await
                .map_err(|_| "capture preparation failed".to_owned())??;
                if control.stop.is_stopped() {
                    return Err("recording preparation cancelled before capture".into());
                }
                session::run_with_control(&root, system, mic, transcription, ceiling, control).await
            }
            SessionCapture::Supervised { plat, clock } => {
                session::run_supervised(&root, plat, clock, transcription, ceiling, control).await
            }
        }
    }
    .await;

    capture_health.close();

    // Persistence remains independent of native inference cleanup: saved audio
    // must be available even if a model never returns. Keep the recording slot
    // while a measured worker still owns native state, so repeated
    // Start requests cannot multiply blocked model instances.
    if speech.has_retained_native_worker() {
        let live = Arc::clone(&live);
        let ready = ready.clone();
        tokio::spawn(async move {
            while speech.has_retained_native_worker() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            clear_session_slot(&live, &ready);
        });
    } else {
        clear_session_slot(&live, &ready);
    }

    if let Err(error) = &outcome {
        ready.fail(error.clone());
    }

    // The meeting's id once it is genuinely in the library. `None` covers a
    // recording that failed and a session that persisted nothing.
    let persisted = match outcome {
        Ok(outcome) => {
            work.require_save();
            // Each entry names its own leg or stage ("mic: …", "capture: …"),
            // and since #79 they are not all transcription failures — a ring
            // drop rides this channel too, because it is the only one anybody
            // reads.
            journal::record(&format!(
                "recording: session ended — {} segment(s), {} degradation(s)",
                outcome.segments.len(),
                outcome.stt_errors.len()
            ));
            // Logged verbatim, and that is a §10 decision rather than an
            // oversight. Every producer of an `stt_errors` entry builds it
            // from buffer counts, device state and deadlines (#79/#81/#85);
            // the one string that comes from outside is `SttError`'s
            // `{provider}: {message}`, which describes the connection, not
            // what was said over it. Its `detail` field — the one documented
            // as diagnostic — is not in that `Display` and does not reach here.
            for e in &outcome.stt_errors {
                diag!("  ! this meeting was degraded: {e}");
            }
            let audit = AuditLog::at(&root);
            if let Err(e) = audit.record(AuditKind::SessionEnd {
                session: outcome.dir.display().to_string(),
                duration_ms: now_ms().saturating_sub(started_at_ms),
            }) {
                diag!("  ! could not write the audit log: {e}");
            }
            // Blocking work — Opus encoding and SQLite — off the runtime.
            let root2 = root.clone();
            tokio::task::spawn_blocking(move || {
                let persisted = (finish)(&root2, &outcome);
                if persisted.is_some() {
                    work.saved();
                }
                // The blocking owner retains this lease even if its awaiting
                // async task is cancelled or unwinds before the save returns.
                drop(work);
                persisted
            })
            .await
            .ok()
            .flatten()
        }
        Err(e) => {
            if ready.session_id().is_some() || ready.capture_started_at_ms().is_some() {
                work.require_save();
            }
            drop(work);
            diag!("  ! the recording failed: {e}");
            None
        }
    };

    // Persist/enrichment do not wait for a retained native worker. Once that
    // worker releases, a fresh meeting may start while enrichment continues.
    if let Some(meeting_id) = persisted {
        enrich_and_announce(&root, &meeting_id, &on_ready).await;
    }
}

/// Title, summary and action items for a meeting already in the library.
///
/// Runs in the session tail independently of the native-cleanup watcher, so
/// enrichment cannot retain the recording slot or capture clock. Problems print
/// and cannot fail recording: audio and committed text are on disk first.
///
/// This is the hook #78 asked for, and it is now wired: the invariant is
/// **announce `enriched` immediately after `enrich_meeting` returns, wherever
/// that call lives** — the tap travels with the call if this ever moves again.
/// The row was already listed by the `persisted` announcement minutes earlier;
/// this second one is what puts the real title and summary on a pane the user
/// may already have open. `Finishing` ends before this runs, so the two are
/// separate events and must stay so.
///
/// This call went missing once already — a patch matched stale text, replaced
/// nothing, and asserted nothing — and every dashboard meeting kept its epoch
/// title while the CLI path worked. There is no unit pin, for either statement
/// below: enrichment opens the real library and keychain, which no test may
/// touch. If the enrich call disappears again, the symptom is epoch titles on
/// dashboard meetings while `fotwd record` titles fine; if the announcement
/// does, it is a dashboard that lists the meeting under its epoch title and
/// only shows the real one after a reload.
async fn enrich_and_announce(root: &Path, meeting_id: &str, ready: &ReadyTap) {
    // Announced before the call, not after it: the two LLM calls below take
    // one to three minutes, and #101's whole complaint is that a working
    // enrichment and a dead one both looked like silence for that whole time.
    journal::record(&format!("enrich   : meeting {meeting_id} starting"));
    let started = fotw_store::now_ms();

    let report = crate::enrich::enrich_meeting(root, meeting_id).await;

    // The title goes to stderr and stops there. §10 puts meeting titles on the
    // never-log list beside the transcript they are written from, and this
    // line — `meeting titled: {title}` — was fine only for as long as its
    // destination was a stream nobody keeps. The durable copy gets the id and
    // the length instead; see `journal::meeting_titled`.
    if let Some(title) = &report.title {
        eprintln!("  meeting titled: {title}");
        journal::record(&journal::meeting_titled(meeting_id, title));
    }
    // The same split, for the same reason: a `problems` entry can carry a
    // child process's stderr over a prompt built from the transcript. The text
    // is already stored in `meetings.enrich_detail`, which the API serves and
    // the dashboard renders (#74), so the log names the count and points there.
    for problem in &report.problems {
        eprintln!("  ! enrichment: {problem}");
    }
    journal::record(&journal::meeting_problems(meeting_id, &report.problems));
    journal::record(&format!(
        "enrich   : meeting {meeting_id} finished in {}s",
        (fotw_store::now_ms() - started).max(0) / 1_000
    ));
    // Unconditional: `problems` is non-fatal and a partial enrichment — a
    // title but no summary — is still something the open tab should be shown.
    ready.emit(meeting_id, MeetingReadyReason::Enriched);
}

#[cfg(test)]
mod capture_retry_tests {
    use super::*;
    use fotw_audio::supervisor::{DeliveryState, RecoveryStatus};
    use fotw_web::recorder::{CaptureDelivery, CaptureSource};

    #[tokio::test]
    async fn retry_is_fenced_by_session_identity_source_health_and_stop() {
        let recorder = DaemonRecorder::with_parts(
            PathBuf::from("unused-capture-health-fixture"),
            tokio::runtime::Handle::current(),
            SegmentTap::default(),
            ReadyTap::default(),
            Box::new(|| panic!("this test must not open audio")),
            Box::new(|| panic!("this test must not open transcription")),
            Box::new(|_, _| panic!("this test must not persist anything")),
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        let monitor = crate::capture_health::CaptureMonitor::default();
        let source_status = |state| RecoveryStatus {
            state,
            attempts: 3,
            max_attempts: 3,
            retry_in_ms: None,
        };
        monitor.publish(CaptureSource::System, source_status(DeliveryState::Healthy));
        monitor.publish(CaptureSource::Mic, source_status(DeliveryState::Exhausted));
        let ready = crate::session::ReadySignal::new();
        ready.signal_capture("current-fixture".into(), 1000);
        *recorder.live.lock().unwrap() = Some(Live {
            speech: crate::speech_status::SpeechMonitor::default(),
            capture_health: monitor.clone(),
            ready,
            stop: StopSignal::new(),
            started_at_ms: 1000,
            stopped_at_ms: None,
            errors: SttErrors::new(),
        });
        assert_eq!(
            recorder.status().capture_health.unwrap().system.state,
            CaptureDelivery::Healthy
        );
        assert!(
            recorder
                .retry_capture("old-fixture", CaptureSource::Mic)
                .is_err()
        );
        assert!(!monitor.take_retry(CaptureSource::Mic));
        assert!(
            recorder
                .retry_capture("current-fixture", CaptureSource::System)
                .is_err()
        );
        assert!(
            recorder
                .retry_capture("current-fixture", CaptureSource::Mic)
                .is_ok()
        );
        assert!(monitor.take_retry(CaptureSource::Mic));
        assert!(
            recorder
                .retry_capture("current-fixture", CaptureSource::Mic)
                .is_ok()
        );
        recorder.stop().unwrap();
        assert!(
            !monitor.take_retry(CaptureSource::Mic),
            "Stop cancels a queued retry"
        );
        assert!(
            recorder
                .retry_capture("current-fixture", CaptureSource::Mic)
                .is_err()
        );
        assert_eq!(
            recorder.status().capture_health.unwrap().mic.state,
            CaptureDelivery::Stopped
        );
    }
}

fn stop_live(live: &mut Live) -> RecordingStatus {
    // Set once. A reloaded tab presses Stop again, and a second stop that
    // moved this forward would make the meeting appear to grow after it
    // ended — the clock is frozen at the first Stop or it is not frozen.
    if !live.ready.is_ready() {
        let capture_setup = live.ready.request_stop(&live.stop);
        let message = if capture_setup {
            live.stopped_at_ms.get_or_insert(now_ms());
            "stopping capture setup; any buffered audio will be saved"
        } else {
            "preparation cancelled; waiting for native setup to release its resources"
        }
        .to_owned();
        live.errors.record(message.clone());
        live.ready.fail(message);
        let status = if capture_setup {
            RecordingStatus::starting(true)
        } else {
            RecordingStatus::preparing(true)
        };
        return status
            .with_transcription_error(live.errors.latest())
            .with_speech_status(live.speech.snapshot());
    }
    let ended_at_ms = *live.stopped_at_ms.get_or_insert(now_ms());
    // Trip and return. The task clears the slot once the meeting is on
    // disk; until then status reads `finishing`.
    live.ready.signal_capture_stopped(ended_at_ms);
    live.capture_health.close();
    live.stop.stop();
    let started_at_ms = live
        .ready
        .capture_started_at_ms()
        .unwrap_or(live.started_at_ms);
    let ended_at_ms = live.ready.capture_ended_at_ms().unwrap_or(ended_at_ms);
    RecordingStatus::finishing(started_at_ms, ended_at_ms)
        .with_speech_status(live.speech.snapshot())
        .with_capture_health(live.capture_health.snapshot())
        .with_transcription_error(live.errors.latest())
        .with_session_id(live.ready.session_id())
}

fn live_status(live: &Live, ceiling: Duration) -> RecordingStatus {
    let started_at_ms = live
        .ready
        .capture_started_at_ms()
        .unwrap_or(live.started_at_ms);
    let status = if !live.ready.is_ready() {
        if live.ready.capture_setup_started() {
            RecordingStatus::starting(live.stop.is_stopped())
        } else {
            RecordingStatus::preparing(live.stop.is_stopped())
        }
    } else {
        match live.ready.capture_ended_at_ms().or(live.stopped_at_ms) {
            // Frozen: the tap is shut, so the number cannot grow.
            Some(ended_at_ms) => {
                RecordingStatus::finishing(started_at_ms, ended_at_ms.max(started_at_ms))
            }
            None => {
                RecordingStatus::recording(started_at_ms, now_ms().saturating_sub(started_at_ms))
            }
        }
    }
    // Kept for finishing too: a tab loaded mid-finalize must still see
    // that transcription failed, or an empty transcript is
    // indistinguishable from a quiet meeting exactly when someone goes
    // looking for the words.
    .with_speech_status(live.speech.snapshot())
    .with_capture_health(live.capture_health.snapshot())
    .with_transcription_error(live.errors.latest())
    .with_session_id(live.ready.session_id());
    if live.ready.is_ready() {
        status.with_auto_stop_at_ms(
            started_at_ms.saturating_add(u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX)),
        )
    } else {
        status
    }
}

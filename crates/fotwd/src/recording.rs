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
//! # Why the slot stays occupied while a session finalizes
//!
//! `stop()` trips the signal and returns immediately: finalizing encodes the
//! whole meeting to Opus, and an HTTP handler that waited for that would hang
//! for minutes on a long call. But the slot is not cleared until the meeting
//! is genuinely on disk, so a second Start during finalization is refused
//! rather than opening a second tap on the same device.
//!
//! What that window is *called* was the bug. It used to read `recording`,
//! which is true of the slot and false of everything the user can see: the tap
//! is shut, the meeting cannot get any longer, and the dashboard drew a
//! climbing clock over a microphone that was already closed. Status now reads
//! `finishing` there, with the clock frozen at the length the meeting ended on
//! (#77). The guard is unchanged — `Finishing` is not `Idle`.
//!
//! # Where finishing ends
//!
//! At persist-and-promote, not at the end of enrichment. Titles and summaries
//! are derived work over a meeting that is already safe on disk, and the CLI
//! deadline is 300 s per call with several calls for a chunked meeting, so
//! waiting for them held the slot — and the user's clock — for minutes.
//! [`enrich_and_announce`] therefore runs *after* the slot clears.
//!
//! That is a real concurrency change and this header owns it: enrichment can
//! now overlap live capture of the next meeting, and one meeting's enrichment
//! can overlap another's. It is safe for the library — SQLite runs in WAL with
//! a busy timeout (`fotw-store/src/db.rs`) and enrichment only writes the
//! title and summary rows of a meeting that is already persisted — but it is
//! real CPU beside live Opus encoding and streaming transcription.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fotw_audio::{AudioPlatform, AudioTap, DeviceId, FormatRequest, SystemScope, platform};
use fotw_secrets::KeyStore;
use fotw_shell::StartOrigin;

use fotw_web::{MeetingReadyReason, RecorderControl, RecorderError, RecordingStatus};

use crate::audit::{AuditKind, AuditLog};
use crate::consent::{JurisdictionSignals, Rules};
use crate::secrets;
use crate::session::{
    self, DeepgramLegs, SegmentTap, SessionControl, SessionOutcome, StopSignal, SttErrors,
    Transcription,
};

/// How a session acquires its taps.
///
/// A function rather than a hardcoded call to [`platform::host`] so the state
/// machine can be exercised on a CI runner with no audio device, using the
/// same `FileAudioSource` the session tests use.
pub type TapOpener =
    Box<dyn Fn() -> Result<(Box<dyn AudioTap>, Option<Box<dyn AudioTap>>), String> + Send + Sync>;

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
type ReadyFn = dyn Fn(&str, MeetingReadyReason) + Send + Sync;

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
        Self(Some(Arc::new(f)))
    }

    /// Announce one meeting, or do nothing when no tap was set.
    pub fn emit(&self, meeting_id: &str, reason: MeetingReadyReason) {
        if let Some(f) = &self.0 {
            f(meeting_id, reason);
        }
    }
}

/// A recording in flight, or one still writing itself to disk.
struct Live {
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

/// Starts and stops real recordings on the UI's behalf.
pub struct DaemonRecorder {
    root: PathBuf,
    handle: tokio::runtime::Handle,
    open_taps: TapOpener,
    transcription: TranscriptionFactory,
    finish: Arc<Finisher>,
    ceiling: Duration,
    ready_deadline: Duration,
    /// Handed to every session for the live transcript (#61).
    on_segment: SegmentTap,
    /// Told when a meeting lands in the library, so an open tab can refetch
    /// without being reloaded (#78).
    on_ready: ReadyTap,
    live: Arc<Mutex<Option<Live>>>,
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
/// retention indefinitely as well as fill the disk. Eight hours is longer than
/// any meeting and shorter than a weekend.
pub const UI_CEILING: Duration = Duration::from_secs(8 * 60 * 60);

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
        Self::with_parts(
            root,
            handle,
            on_segment,
            on_ready,
            Box::new(|| {
                let plat = platform::host();
                let system = plat
                    .open_system(SystemScope::DefaultOutputMix, FormatRequest::any())
                    .map_err(|e| format!("could not open the system tap: {e}"))?;
                // Optional on purpose: a machine with no input device should
                // still record the far end rather than refuse to start.
                let mic = plat
                    .open_mic(&DeviceId::new("default"), FormatRequest::any())
                    .ok();
                Ok((system, mic))
            }),
            Box::new(keychain_transcription),
            Box::new(move |root, outcome| persist_and_promote(root, outcome, &finish_ready)),
            UI_CEILING,
            READY_DEADLINE,
        )
    }

    /// [`DaemonRecorder::new`] with every dependency named, for tests.
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
        Self {
            root,
            handle,
            open_taps,
            transcription,
            finish: Arc::new(finish),
            ceiling,
            ready_deadline,
            on_segment,
            on_ready,
            live: Arc::new(Mutex::new(None)),
        }
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Look the Deepgram key up in the OS keychain, as `fotwd record` does.
///
/// Only the system leg is transcribed: the mic leg needs its own connection
/// and doubles the bill, which is the explicit decision in spec 7.5 rather
/// than a default.
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
        eprintln!("  ! could not append the transcript: {e}");
    }

    let data_root = root.parent().unwrap_or(root).to_path_buf();
    // A dated placeholder, not an epoch second: enrichment replaces it within
    // seconds when there is speech to name, and a meeting recorded in silence
    // keeps this forever (#76, and #67's transcript-less acceptance).
    let title = crate::enrich::dated_fallback_title(fotw_store::now_ms());

    match crate::open_library(root) {
        Err(e) => {
            eprintln!("  ! could not open the library: {e}");
            None
        }
        Ok(mut db) => match crate::persist::persist_session(&mut db, outcome, &title) {
            Err(e) => {
                eprintln!("  ! could not add to the library: {e}");
                None
            }
            Ok(id) => {
                // Before promotion, on purpose — see this function's header.
                ready.emit(&id, MeetingReadyReason::Persisted);
                if let Err(e) = crate::retention::promote_session(
                    &mut db,
                    &data_root,
                    &outcome.dir,
                    &id,
                    outcome.started_at_ms,
                ) {
                    eprintln!("  ! could not archive the session: {e}");
                }
                Some(id)
            }
        },
    }
}

impl RecorderControl for DaemonRecorder {
    fn start(&self) -> Result<RecordingStatus, RecorderError> {
        let mut slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(RecorderError::AlreadyRecording);
        }

        // CON-05: the gate runs before anything is captured. The HTTP layer
        // has already refused a start with no acknowledgement, so reaching
        // here means a human ticked the box; what is recorded here is *what
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
                origin: StartOrigin::WebUi.label().to_owned(),
                detected_app: None,
                jurisdiction_warning: escalation.user_text(),
                acknowledged_all_party: escalation.blocks(),
            })
            .map_err(|e| {
                // A recording we cannot account for is not one we should make.
                RecorderError::Failed(format!("could not write the audit log: {e}"))
            })?;

        let (system, mic) = (self.open_taps)().map_err(RecorderError::Failed)?;

        let transcription = (self.transcription)();

        let mut control = SessionControl::new();
        control.on_segment = self.on_segment.clone();
        let stop = control.stop.clone();
        let ready = control.ready.clone();
        let started_at_ms = now_ms();
        *slot = Some(Live {
            stop: stop.clone(),
            started_at_ms,
            stopped_at_ms: None,
            errors: control.errors.clone(),
        });
        drop(slot);

        let root = self.root.clone();
        let live = Arc::clone(&self.live);
        let ceiling = self.ceiling;
        // Behind an `Arc` so the task owns a handle without the recorder
        // having to be `Clone`.
        let finish = Arc::clone(&self.finish);

        self.handle.spawn(spawn_session(
            root,
            system,
            mic,
            transcription,
            ceiling,
            control,
            started_at_ms,
            Arc::clone(&live),
            finish,
            self.on_ready.clone(),
        ));

        // Do not answer until capture is real. Answering on spawn is what let
        // a wedged device show a RECORDING badge over an empty disk.
        if !ready.wait_timeout(self.ready_deadline) {
            stop.stop();
            *live.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Err(RecorderError::Failed(
                "the audio device did not start. This is usually a Core Audio \
                 HAL that still believes a dead client holds the device; it \
                 blocks rather than failing, and nothing in this process can \
                 cancel it. Restart the audio daemon with `sudo killall \
                 coreaudiod` (this briefly interrupts all audio) and try again."
                    .to_owned(),
            ));
        }

        Ok(RecordingStatus::recording(started_at_ms, 0))
    }

    fn stop(&self) -> Result<RecordingStatus, RecorderError> {
        let mut slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = slot.as_mut() else {
            return Err(RecorderError::NotRecording);
        };
        // Set once. A reloaded tab presses Stop again, and a second stop that
        // moved this forward would make the meeting appear to grow after it
        // ended — the clock is frozen at the first Stop or it is not frozen.
        let ended_at_ms = *live.stopped_at_ms.get_or_insert(now_ms());
        // Trip and return. The task clears the slot once the meeting is on
        // disk; until then status reads `finishing`.
        live.stop.stop();
        Ok(RecordingStatus::finishing(live.started_at_ms, ended_at_ms)
            .with_transcription_error(live.errors.latest()))
    }

    fn status(&self) -> RecordingStatus {
        let slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map_or_else(RecordingStatus::idle, |live| {
            match live.stopped_at_ms {
                // Frozen: the tap is shut, so the number cannot grow.
                Some(ended_at_ms) => RecordingStatus::finishing(live.started_at_ms, ended_at_ms),
                None => RecordingStatus::recording(
                    live.started_at_ms,
                    now_ms().saturating_sub(live.started_at_ms),
                ),
            }
            // Kept for finishing too: a tab loaded mid-finalize must still see
            // that transcription failed, or an empty transcript is
            // indistinguishable from a quiet meeting exactly when someone goes
            // looking for the words.
            .with_transcription_error(live.errors.latest())
        })
    }
}

/// Run one session and clear the slot when it is genuinely finished.
#[allow(clippy::too_many_arguments)]
async fn spawn_session(
    root: PathBuf,
    system: Box<dyn AudioTap>,
    mic: Option<Box<dyn AudioTap>>,
    transcription: Transcription,
    ceiling: Duration,
    control: SessionControl,
    started_at_ms: u64,
    live: Arc<Mutex<Option<Live>>>,
    finish: Arc<Finisher>,
    on_ready: ReadyTap,
) {
    let outcome =
        session::run_with_control(&root, system, mic, transcription, ceiling, control).await;

    // The meeting's id once it is genuinely in the library. `None` covers a
    // recording that failed and a session that persisted nothing.
    let persisted = match outcome {
        Ok(outcome) => {
            // Each entry names its own leg or stage ("mic: …", "capture: …"),
            // and since #79 they are not all transcription failures — a ring
            // drop rides this channel too, because it is the only one anybody
            // reads.
            for e in &outcome.stt_errors {
                eprintln!("  ! this meeting was degraded: {e}");
            }
            let audit = AuditLog::at(&root);
            if let Err(e) = audit.record(AuditKind::SessionEnd {
                session: outcome.dir.display().to_string(),
                duration_ms: now_ms().saturating_sub(started_at_ms),
            }) {
                eprintln!("  ! could not write the audit log: {e}");
            }
            // Blocking work — Opus encoding and SQLite — off the runtime.
            let root2 = root.clone();
            tokio::task::spawn_blocking(move || (finish)(&root2, &outcome))
                .await
                .ok()
                .flatten()
        }
        Err(e) => {
            eprintln!("  ! the recording failed: {e}");
            None
        }
    };

    // Finishing ends here. The meeting is persisted and promoted, so the slot
    // can free and the dashboard can stop saying "finishing…" — everything
    // below is derived work over a file that is already safe (#77).
    *live.lock().unwrap_or_else(|e| e.into_inner()) = None;

    if let Some(meeting_id) = persisted {
        enrich_and_announce(&root, &meeting_id, &on_ready).await;
    }
}

/// Title, summary and action items for a meeting already in the library.
///
/// Runs in the tail of the session task, after the recorder's slot has been
/// freed, so nothing here can hold the user's clock (#77). Problems print and
/// nothing can fail the recording: the audio and the transcript are on disk
/// before this is called.
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
    let report = crate::enrich::enrich_meeting(root, meeting_id).await;
    if let Some(title) = &report.title {
        eprintln!("  meeting titled: {title}");
    }
    for problem in &report.problems {
        eprintln!("  ! enrichment: {problem}");
    }
    // Unconditional: `problems` is non-fatal and a partial enrichment — a
    // title but no summary — is still something the open tab should be shown.
    ready.emit(meeting_id, MeetingReadyReason::Enriched);
}

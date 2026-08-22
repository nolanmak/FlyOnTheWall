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
//! for minutes on a long call. But the slot is not cleared until the task is
//! genuinely done, so a second Start during finalization is refused rather
//! than opening a second tap on the same device. Status reads `recording`
//! until the session is fully on disk, which is also the honest answer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fotw_audio::{AudioPlatform, AudioTap, DeviceId, FormatRequest, SystemScope, platform};
use fotw_secrets::KeyStore;
use fotw_shell::StartOrigin;

use fotw_web::{RecorderControl, RecorderError, RecordingStatus};

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
pub type Finisher = Box<dyn Fn(&Path, &SessionOutcome) + Send + Sync>;

/// How a session decides whether, and how, to transcribe.
///
/// Injectable because the real one reads the OS keychain, and a test that
/// reads the *user's* keychain is not merely impure — it raises an approval
/// dialog. `cargo` re-signs a test binary ad-hoc on every build, so each run
/// presents a new code identity to an item whose ACL is bound to the identity
/// that created it, and macOS prompts every single time.
pub type TranscriptionFactory = Box<dyn Fn() -> Transcription + Send + Sync>;

/// A recording in flight, or one still writing itself to disk.
struct Live {
    stop: StopSignal,
    started_at_ms: u64,
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
    pub fn new(root: PathBuf, handle: tokio::runtime::Handle, on_segment: SegmentTap) -> Self {
        Self::with_parts(
            root,
            handle,
            on_segment,
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
            Box::new(persist_and_promote),
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
fn persist_and_promote(root: &Path, outcome: &SessionOutcome) {
    if !outcome.segments.is_empty()
        && let Err(e) = session::append_segments(&outcome.dir, &outcome.segments)
    {
        eprintln!("  ! could not append the transcript: {e}");
    }

    let data_root = root.parent().unwrap_or(root).to_path_buf();
    let title = format!(
        "Untitled recording — {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    );

    match crate::open_library(root) {
        Err(e) => eprintln!("  ! could not open the library: {e}"),
        Ok(mut db) => match crate::persist::persist_session(&mut db, outcome, &title) {
            Err(e) => eprintln!("  ! could not add to the library: {e}"),
            Ok(id) => {
                if let Err(e) = crate::retention::promote_session(
                    &mut db,
                    &data_root,
                    &outcome.dir,
                    &id,
                    outcome.started_at_ms,
                ) {
                    eprintln!("  ! could not archive the session: {e}");
                }
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
        let slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = slot.as_ref() else {
            return Err(RecorderError::NotRecording);
        };
        // Trip and return. The task clears the slot once the meeting is on
        // disk; until then status honestly still reads `recording`.
        live.stop.stop();
        Ok(
            RecordingStatus::recording(
                live.started_at_ms,
                now_ms().saturating_sub(live.started_at_ms),
            )
            .with_transcription_error(live.errors.latest()),
        )
    }

    fn status(&self) -> RecordingStatus {
        let slot = self.live.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map_or_else(RecordingStatus::idle, |live| {
            RecordingStatus::recording(
                live.started_at_ms,
                now_ms().saturating_sub(live.started_at_ms),
            )
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
) {
    let outcome =
        session::run_with_control(&root, system, mic, transcription, ceiling, control).await;

    match outcome {
        Ok(outcome) => {
            for e in &outcome.stt_errors {
                eprintln!("  ! transcription failed during this meeting: {e}");
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
            let _ =
                tokio::task::spawn_blocking(move || (finish)(&root2, &outcome)).await;
        }
        Err(e) => eprintln!("  ! the recording failed: {e}"),
    }

    *live.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

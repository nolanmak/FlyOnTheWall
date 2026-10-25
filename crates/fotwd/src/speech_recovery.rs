//! App-owned local recovery of a saved capture, independent of current ASR policy.
use crate::failover::{
    coverage::{CoverageJournal, LocalPlan},
    stream::{FailoverStream, LocalResumeConfig},
};
use fotw_pipeline::replay::{ReplayReader, ReplaySource, ReplayWriter};
use fotw_store::Db;
use fotw_stt::{RecognizerFactory, Source};
use fotw_web::speech_recovery::{
    RecoveryError, RecoveryPhase, RecoverySource, RecoveryStatus, SpeechRecoveryControl,
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::time::{Duration, Instant};

/// Installed factory resolver. Called on an owned worker without a database lock.
pub type ModelResolver =
    dyn Fn(&LocalPlan, &Path) -> Result<Arc<dyn RecognizerFactory>, String> + Send + Sync;

/// Lifetime of the bounded, local-only startup pass.
pub struct StartupRecovery {
    owner: SavedSpeechRecovery,
    running: Arc<AtomicBool>,
}
impl StartupRecovery {
    /// Whether the startup pass still owns scheduling work.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}
impl Drop for StartupRecovery {
    fn drop(&mut self) {
        let state = self.owner.state();
        state.startup_stop.store(true, Ordering::Release);
        if let Some(job) = &state.job
            && job.automatic
            && job.status.worker_running
        {
            let _ = job
                .stop
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}
struct StartupRunning(Arc<AtomicBool>);
impl Drop for StartupRunning {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Job {
    meeting: String,
    automatic: bool,
    status: RecoveryStatus,
    entered: Duration,
    stop: Arc<AtomicU8>, // 1: explicit cancellation, 2: deadline
    observed: Option<(Source, crate::failover::stream::StatusReader)>,
}
#[derive(Default)]
struct State {
    job: Option<Job>,
    startup_started: bool,
    startup_stop: Arc<AtomicBool>,
}

/// One saved-audio job at a time; no capture or cloud-credential capability.
#[derive(Clone)]
pub struct SavedSpeechRecovery {
    db: Arc<Mutex<Db>>,
    data_root: PathBuf,
    resolver: Arc<ModelResolver>,
    prepare_timeout: Duration,
    progress_timeout: Duration,
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
    hub: Option<Arc<fotw_web::DeltaHub>>,
}
impl std::fmt::Debug for SavedSpeechRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SavedSpeechRecovery(<redacted>)")
    }
}
struct SavedSource {
    source: Source,
    plan: Option<LocalPlan>,
    pending: bool,
    complete: bool,
    end: u64,
}
struct SavedCapture {
    dir: PathBuf,
    session: String,
    model_path: PathBuf,
    sources: Vec<SavedSource>,
    status: RecoveryStatus,
    capture_owner: Option<fotw_pipeline::capture_clock::CaptureOwner>,
    prefix_only: bool,
    started: u64,
    observed_elapsed: u64,
}
fn initial(phase: RecoveryPhase) -> RecoveryStatus {
    RecoveryStatus {
        phase,
        sources: vec![],
        worker_running: false,
        can_retry: phase != RecoveryPhase::Complete,
        message: None,
    }
}
fn regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}
impl SavedSpeechRecovery {
    /// Register manual controls and start the same owned local startup pass.
    pub fn install(
        mut self,
        state: &fotw_web::AppState,
    ) -> std::io::Result<Option<StartupRecovery>> {
        self.hub = Some(state.hub().clone());
        let owner = Arc::new(self);
        state.set_speech_recovery(owner.clone());
        owner.start_pending()
    }
    /// Start one local-only pass over saved recordings, off the server runtime.
    pub fn start_pending(&self) -> std::io::Result<Option<StartupRecovery>> {
        let stop = {
            let mut state = self.state();
            if state.startup_started || state.startup_stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            state.startup_started = true;
            state.startup_stop.clone()
        };
        let running = Arc::new(AtomicBool::new(true));
        let completion = StartupRunning(running.clone());
        let owned = self.clone();
        let worker = std::thread::Builder::new()
            .name("startup-speech-recovery".into())
            .spawn(move || {
                let _completion = completion;
                if owned.pending_pass(&stop).is_err() {
                    crate::journal::record(
                        "speech recovery: startup scan could not finish; saved audio retained",
                    );
                }
            });
        if let Err(error) = worker {
            self.state().startup_started = false;
            return Err(error);
        }
        Ok(Some(StartupRecovery {
            owner: self.clone(),
            running,
        }))
    }
    fn pending_pass(&self, stop: &AtomicBool) -> Result<(), RecoveryError> {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        self.import_stopped_captures(stop)?;
        // A fixed upper boundary excludes later sessions, with at most one
        // page of IDs retained. Filesystem and native work never own this SQL lock.
        let upper: Option<String> = self
            .db()
            .conn()
            .query_row("SELECT max(session_id) FROM session_note_links", [], |r| {
                r.get(0)
            })
            .map_err(|_| RecoveryError::Invalid)?;
        let Some(upper) = upper else {
            return Ok(());
        };
        let mut cursor = String::new();
        while !stop.load(Ordering::Acquire) {
            let page = {
                let db = self.db();
                let mut query = db.conn().prepare("SELECT session_id,meeting_id FROM session_note_links WHERE session_id>?1 AND session_id<=?2 ORDER BY session_id LIMIT 32")
                    .map_err(|_| RecoveryError::Invalid)?;
                let rows = query
                    .query_map([&cursor, &upper], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(|_| RecoveryError::Invalid)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(|_| RecoveryError::Invalid)?
            };
            if page.is_empty() {
                break;
            }
            for (session, meeting) in page {
                cursor = session;
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                if !self.load(&meeting).is_ok_and(|capture| {
                    capture.status.phase == RecoveryPhase::Incomplete
                        || (capture.prefix_only
                            && capture.status.can_retry
                            && !capture
                                .sources
                                .iter()
                                .any(|s| s.pending && s.plan.is_some()))
                }) {
                    continue;
                }
                match self.start_job(&meeting, true) {
                    Ok(_) => {}
                    // Explicit user control owns the pass from here. Never
                    // queue another native owner behind a retained worker.
                    Err(RecoveryError::Busy) => return Ok(()),
                    Err(_) => continue,
                }
                loop {
                    self.check();
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if !self
                        .state()
                        .job
                        .as_ref()
                        .is_some_and(|j| j.status.worker_running)
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        Ok(())
    }
    fn import_stopped_captures(&self, stop: &AtomicBool) -> Result<(), RecoveryError> {
        let sessions = crate::retention::sessions_dir(&self.data_root);
        match std::fs::symlink_metadata(&sessions) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(m) if m.file_type().is_dir() => {}
            _ => return Err(RecoveryError::Invalid),
        }
        let entries = std::fs::read_dir(sessions).map_err(|_| RecoveryError::Invalid)?;
        let mut needs_reconciliation = false;
        let mut projection_failed = false;
        for entry in entries {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_ok_and(|m| m.is_dir()) {
                continue;
            }
            if entry.file_name().to_str().is_some_and(|session| {
                self.db()
                    .session_note_meeting(session)
                    .is_ok_and(|id| id.is_some())
            }) {
                continue;
            }
            match crate::interrupted_capture::prepare(&entry.path(), stop) {
                Ok(Some(capture)) => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let outcome = crate::session::SessionOutcome {
                        dir: capture.dir.clone(),
                        started_at_ms: capture.started,
                        capture_ended_at_ms: capture.ended,
                        ..Default::default()
                    };
                    let newly_indexed = self
                        .db()
                        .session_note_meeting(&capture.session)
                        .is_ok_and(|id| id.is_none());
                    let projected = if capture.ended.is_some() {
                        crate::persist::persist_session(
                            &mut self.db(),
                            &outcome,
                            "Recovered recording",
                        )
                    } else {
                        crate::persist::persist_interrupted(
                            &mut self.db(),
                            &capture.dir,
                            capture.started,
                            capture.observed_elapsed,
                        )
                    };
                    match projected {
                        Ok(id) if newly_indexed => {
                            if let Some(hub) = &self.hub {
                                // Refresh the library without registering old sessions
                                // in the live capture's bounded replay history.
                                hub.announce_meeting_ready(
                                    &id,
                                    fotw_web::MeetingReadyReason::Persisted,
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(_) => projection_failed = true,
                    }
                }
                Ok(None) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => needs_reconciliation = true,
            }
        }
        if projection_failed {
            crate::journal::record(
                "speech recovery: interrupted capture projection failed; audio retained",
            );
        }
        if needs_reconciliation {
            crate::journal::record(
                "speech recovery: interrupted captures require further reconciliation; original audio retained",
            );
        }
        Ok(())
    }
    /// Production resolver checks the saved revision, never the current provider.
    pub fn new(db: Db, data_root: PathBuf) -> Self {
        let models = data_root.clone();
        Self::with_parts(
            db,
            data_root,
            Arc::new(move |plan, candidate| {
                crate::stt::saved_local_model(plan, candidate, &models)
            }),
            Duration::from_secs(30),
            Duration::from_secs(120),
        )
    }
    /// Injection seam for device-free recovery tests.
    pub fn with_parts(
        db: Db,
        data_root: PathBuf,
        resolver: Arc<ModelResolver>,
        prepare_timeout: Duration,
        progress_timeout: Duration,
    ) -> Self {
        let origin = Instant::now();
        Self {
            db: Arc::new(Mutex::new(db)),
            data_root,
            resolver,
            prepare_timeout,
            progress_timeout,
            state: Arc::new(Mutex::new(State::default())),
            clock: Arc::new(move || origin.elapsed()),
            hub: None,
        }
    }
    /// Inject a monotonic clock before the first job, for deterministic deadline tests.
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> Duration + Send + Sync>) -> Self {
        assert!(self.state().job.is_none());
        self.clock = clock;
        self
    }
    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn load(&self, meeting: &str) -> Result<SavedCapture, RecoveryError> {
        self.load_with_owner(meeting, None)
    }
    fn load_with_owner(
        &self,
        meeting: &str,
        held_capture: Option<&fotw_pipeline::capture_clock::CaptureOwner>,
    ) -> Result<SavedCapture, RecoveryError> {
        // Release SQL ownership before any filesystem or native work.
        let (session, started, elapsed, clock_is_final, model_path) = {
            let mut db = self.db();
            db.meetings()
                .get(meeting)
                .map_err(|_| RecoveryError::NotFound)?;
            let mut query = db.conn().prepare("SELECT session_id,started_at_ms,elapsed_ms,clock_is_final FROM session_note_links WHERE meeting_id=?1 LIMIT 2").map_err(|_| RecoveryError::Invalid)?;
            let rows = query
                .query_map([meeting], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })
                .map_err(|_| RecoveryError::Invalid)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| RecoveryError::Invalid)?;
            if rows.len() != 1 {
                return Err(RecoveryError::NotFound);
            }
            let (session, start, elapsed, clock_is_final) = rows.into_iter().next().unwrap();
            let candidate = crate::stt::SttSettings::read(&db)
                .ok()
                .map(|s| s.model_path(&self.data_root))
                .unwrap_or_default();
            (session, start, elapsed, clock_is_final, candidate)
        };
        if started < 0
            || elapsed < 0
            || started.checked_add(elapsed).is_none()
            || session.is_empty()
            || session.len() > 128
            || !session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(RecoveryError::Invalid);
        }
        let sessions = crate::retention::sessions_dir(&self.data_root);
        let dir = sessions.join(&session);
        for path in [&sessions, &dir] {
            if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                return Err(RecoveryError::NotFound);
            }
        }
        if !regular(&dir.join("manifest.json")) {
            return Err(RecoveryError::Invalid);
        }
        let wal =
            fotw_pipeline::wal::SessionState::read(&dir).map_err(|_| RecoveryError::Invalid)?;
        if wal.manifest.id != session || wal.manifest.started_at_ms != started as u64 {
            return Err(RecoveryError::Invalid);
        }
        // An unfinished WAL must prove the original Stop. Saved finalized clocks
        // remain authoritative, with modern capture ownership retained through
        // native cleanup too. Legacy indexed captures still use source leases.
        let capture_owner = if !clock_is_final {
            if let Some(owner) = held_capture {
                crate::interrupted_capture::verify_retained(&dir, owner, started as u64)
                    .map_err(|_| RecoveryError::Invalid)?;
                None
            } else {
                Some(
                    crate::interrupted_capture::own_retained(&dir, started as u64).map_err(
                        |e| {
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                RecoveryError::Busy
                            } else {
                                RecoveryError::Invalid
                            }
                        },
                    )?,
                )
            }
        } else if wal.is_finalized() {
            if held_capture.is_some() {
                None
            } else {
                match std::fs::symlink_metadata(dir.join("capture-owner.lock")) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Ok(m) if m.file_type().is_file() => Some(
                        fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).map_err(|e| {
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                RecoveryError::Busy
                            } else {
                                RecoveryError::Invalid
                            }
                        })?,
                    ),
                    _ => return Err(RecoveryError::Invalid),
                }
            }
        } else if let Some(owner) = held_capture {
            crate::interrupted_capture::verify_stopped(
                &dir,
                owner,
                started as u64,
                (started + elapsed) as u64,
            )
            .map_err(|_| RecoveryError::Invalid)?;
            None
        } else {
            Some(
                crate::interrupted_capture::own_stopped(
                    &dir,
                    started as u64,
                    (started + elapsed) as u64,
                )
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        RecoveryError::Busy
                    } else {
                        RecoveryError::Invalid
                    }
                })?,
            )
        };
        let mut sources = Vec::new();
        let mut status = initial(if clock_is_final {
            RecoveryPhase::Complete
        } else {
            RecoveryPhase::Retained
        });
        if !clock_is_final {
            status.can_retry = false;
            status.message=Some("Recording interrupted. The final recording time is unknown. Your audio, notes and any saved transcript are retained.".into());
        }
        for (source, replay, name) in [
            (Source::System, ReplaySource::System, "system"),
            (Source::Mic, ReplaySource::Mic, "mic"),
        ] {
            let journal = dir.join(format!("{name}.stt-coverage.jsonl"));
            if !journal.try_exists().map_err(|_| RecoveryError::Invalid)? {
                continue;
            }
            if !regular(&journal) || !regular(&dir.join(format!("{name}.stt16"))) {
                return Err(RecoveryError::Invalid);
            }
            let snapshot = CoverageJournal::snapshot(&dir, &session, source)
                .map_err(|_| RecoveryError::Invalid)?;
            let plan = snapshot.fallback;
            let reader =
                ReplayReader::open(&dir, &session, replay).map_err(|_| RecoveryError::Invalid)?;
            reader.sealed_end().map_err(|_| RecoveryError::Invalid)?;
            let end = if clock_is_final {
                reader.saved_end()
            } else {
                reader.complete_prefix_end()
            }
            .map_err(|_| RecoveryError::Invalid)?;
            let through = snapshot
                .commits
                .last()
                .map_or(reader.first_sample(), |c| c.through_sample);
            if through > end {
                return Err(RecoveryError::Invalid);
            }
            let complete =
                reader.is_complete().map_err(|_| RecoveryError::Invalid)? && through == end;
            if clock_is_final && through < end && plan.is_none() {
                return Err(RecoveryError::Invalid);
            }
            let engine = plan
                .as_ref()
                .map(|plan| &plan.engine)
                .or_else(|| snapshot.epochs.last().map(|epoch| &epoch.engine))
                .ok_or(RecoveryError::Invalid)?;
            if clock_is_final
                && (!complete || !reader.is_projected().map_err(|_| RecoveryError::Invalid)?)
            {
                status.phase = RecoveryPhase::Incomplete;
                status.can_retry = true;
            } else if !clock_is_final
                && ((through < end && plan.is_some())
                    || !reader
                        .is_prefix_projected(through, snapshot.valid_bytes)
                        .map_err(|_| RecoveryError::Invalid)?)
            {
                status.can_retry = true;
            }
            status.sources.push(RecoverySource {
                source: name.into(),
                model: engine.model.clone(),
                revision: engine.revision.clone(),
                pending_audio_ms: (end - through) / 16,
            });
            sources.push(SavedSource {
                source,
                plan,
                pending: through < end,
                complete,
                end,
            });
        }
        if sources.is_empty() && clock_is_final {
            return Err(RecoveryError::NotFound);
        }
        if !clock_is_final
            && status.can_retry
            && let Some(message) = status.message.as_mut()
        {
            message.push_str(
                " Available saved speech can be recovered locally; other audio remains pending.",
            );
        }
        if !clock_is_final
            && !status.can_retry
            && !sources.is_empty()
            && sources.iter().all(|s| !s.pending)
        {
            status.phase = RecoveryPhase::Partial;
            status.message=Some("Available saved speech is accounted for. The final recording time is unknown and additional original audio is retained.".into());
        }
        Ok(SavedCapture {
            dir,
            session,
            model_path,
            sources,
            status,
            capture_owner,
            prefix_only: !clock_is_final,
            started: started as u64,
            observed_elapsed: elapsed as u64,
        })
    }
    fn start_job(&self, meeting: &str, automatic: bool) -> Result<RecoveryStatus, RecoveryError> {
        if meeting.is_empty() || meeting.len() > 128 {
            return Err(RecoveryError::NotFound);
        }
        let retained: bool = self.db().conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM session_note_links WHERE meeting_id=?1 AND clock_is_final=0)",
            [meeting], |r| r.get(0)).map_err(|_| RecoveryError::Invalid)?;
        if retained && !self.load(meeting)?.status.can_retry {
            return Err(RecoveryError::Invalid);
        }
        {
            let mut state = self.state();
            if automatic {
                if state.startup_stop.load(Ordering::Acquire) {
                    return Err(RecoveryError::Busy);
                }
            } else {
                state.startup_stop.store(true, Ordering::Release);
            }
            if state
                .job
                .as_ref()
                .is_some_and(|job| job.status.worker_running)
            {
                return Err(RecoveryError::Busy);
            }
            state.job = Some(Job {
                meeting: meeting.into(),
                automatic,
                status: RecoveryStatus {
                    worker_running: true,
                    can_retry: false,
                    ..initial(RecoveryPhase::Preparing)
                },
                entered: (self.clock)(),
                stop: Arc::new(AtomicU8::new(0)),
                observed: None,
            });
        }
        let owned = self.clone();
        let id = meeting.to_owned();
        let worker = std::thread::Builder::new()
            .name("saved-speech-recovery".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut capture = owned.load(&id)?;
                    if capture.status.phase == RecoveryPhase::Retained && !capture.status.can_retry
                    {
                        return Err(RecoveryError::Invalid);
                    }
                    // Declare this BEFORE the runtime: unwinding drops the
                    // runtime and joins blocking cleanup before releasing it.
                    let capture_owner = capture.capture_owner.take();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| RecoveryError::Invalid)?;
                    let result =
                        runtime.block_on(owned.recover(&id, capture, capture_owner.as_ref()));
                    // Runtime drop joins any owned blocking cleanup before admission
                    // is released, including a late constructor or file flush.
                    drop(runtime);
                    drop(capture_owner);
                    result
                }))
                .unwrap_or(Err(RecoveryError::Invalid));
                owned.ended(result);
            });
        if worker.is_err() {
            self.ended(Err(RecoveryError::Invalid));
            return Err(RecoveryError::Invalid);
        }
        self.read(meeting)
    }
    fn stage(&self, phase: RecoveryPhase, sources: Option<Vec<RecoverySource>>) {
        if let Some(job) = self.state().job.as_mut() {
            job.entered = (self.clock)();
            job.status.phase = phase;
            if matches!(phase, RecoveryPhase::Preparing | RecoveryPhase::Complete) {
                job.observed = None;
            }
            if let Some(sources) = sources {
                job.status.sources = sources;
            }
        }
    }
    fn check(&self) -> bool {
        let mut state = self.state();
        let Some(job) = state.job.as_mut() else {
            return true;
        };
        if let Some((source, reader)) = job.observed.clone() {
            Self::update_progress(job, source, &reader, (self.clock)());
        }
        let limit = match job.status.phase {
            RecoveryPhase::Preparing => Some(self.prepare_timeout),
            RecoveryPhase::Transcribing => Some(self.progress_timeout),
            _ => None,
        };
        if limit.is_some_and(|limit| (self.clock)().saturating_sub(job.entered) >= limit) {
            let _ = job
                .stop
                .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
        }
        let stopped = job.stop.load(Ordering::Acquire) != 0;
        if stopped && job.status.worker_running {
            job.status.phase = RecoveryPhase::Stopping;
            job.status.message = Some(
                "Local recovery stopped; waiting for owned work to return. Audio retained.".into(),
            );
        }
        stopped
    }
    fn update_progress(
        job: &mut Job,
        source: Source,
        reader: &crate::failover::stream::StatusReader,
        now: Duration,
    ) {
        let lag = reader.read().lag_samples / 16;
        let name = match source {
            Source::System => "system",
            Source::Mic => "mic",
        };
        if let Some(source) = job.status.sources.iter_mut().find(|s| s.source == name) {
            // Only a durably committed coverage advance renews the stall budget.
            // Status polling, native activity and uncommitted buffers do not.
            if lag < source.pending_audio_ms
                && job.status.phase == RecoveryPhase::Transcribing
                && job.stop.load(Ordering::Acquire) == 0
            {
                job.entered = now;
            }
            source.pending_audio_ms = lag;
        }
    }
    fn progress(&self, source: Source, reader: &crate::failover::stream::StatusReader) {
        if let Some(job) = self.state().job.as_mut() {
            Self::update_progress(job, source, reader, (self.clock)());
        }
    }
    fn complete_confirmed_source(
        &self,
        dir: &Path,
        session: &str,
        source: Source,
    ) -> Result<(), RecoveryError> {
        let replay = match source {
            Source::System => ReplaySource::System,
            Source::Mic => ReplaySource::Mic,
        };
        let mut audio =
            ReplayWriter::resume(dir, session, replay).map_err(|_| RecoveryError::Invalid)?;
        let journal =
            CoverageJournal::open(dir, session, source).map_err(|_| RecoveryError::Invalid)?;
        if journal.committed_through() != audio.end_sample() || self.check() {
            return Err(RecoveryError::Invalid);
        }
        let end = audio.seal().map_err(|_| RecoveryError::Invalid)?;
        let mut reader =
            ReplayReader::open(dir, session, replay).map_err(|_| RecoveryError::Invalid)?;
        reader
            .mark_complete(end)
            .map_err(|_| RecoveryError::Invalid)
    }
    fn ended(&self, result: Result<RecoveryPhase, RecoveryError>) {
        if let Some(job) = self.state().job.as_mut() {
            job.status.worker_running = false;
            job.status.phase = match job.stop.load(Ordering::Acquire) {
                1 => RecoveryPhase::Cancelled,
                2 => RecoveryPhase::Failed,
                _ if result.is_ok() => result.unwrap(),
                _ => RecoveryPhase::Failed,
            };
            job.status.can_retry = !matches!(
                job.status.phase,
                RecoveryPhase::Complete | RecoveryPhase::Partial
            );
            job.status.message = match job.status.phase {
                RecoveryPhase::Complete => Some("Saved speech recovered locally. Existing corrections and recording times preserved.".into()),
                RecoveryPhase::Partial => Some("Available saved speech recovered locally. The final recording time remains unknown and additional original audio is retained.".into()),
                RecoveryPhase::Cancelled => Some("Local recovery cancelled. Audio retained for retry.".into()),
                _ if job.stop.load(Ordering::Acquire) == 2 => Some("Local recovery made no timely preparation or durable transcription progress. Audio retained for retry.".into()),
                _ => Some("Local recovery could not finish. Check that the original model revision is installed, then retry; audio retained.".into()),
            };
        }
    }
    async fn recover(
        &self,
        meeting: &str,
        capture: SavedCapture,
        held_capture: Option<&fotw_pipeline::capture_clock::CaptureOwner>,
    ) -> Result<RecoveryPhase, RecoveryError> {
        self.stage(RecoveryPhase::Preparing, Some(capture.status.sources));
        for saved in capture.sources {
            if self.check() {
                return Err(RecoveryError::Invalid);
            }
            if saved.complete || (capture.prefix_only && (!saved.pending || saved.plan.is_none())) {
                continue;
            }
            self.stage(RecoveryPhase::Preparing, None);
            if !saved.pending {
                self.complete_confirmed_source(&capture.dir, &capture.session, saved.source)?;
                continue;
            }
            let plan = saved.plan.ok_or(RecoveryError::Invalid)?;
            let factory =
                (self.resolver)(&plan, &capture.model_path).map_err(|_| RecoveryError::Invalid)?;
            let stop = self.state().job.as_ref().unwrap().stop.clone();
            let factory = Arc::new(GuardedFactory {
                inner: factory,
                stop,
            }) as Arc<dyn RecognizerFactory>;
            if self.check() {
                return Err(RecoveryError::Invalid);
            }
            let request = LocalResumeConfig {
                session_id: capture.session.clone(),
                source: saved.source,
                local: factory,
                revision: plan.engine.revision,
                stop_timeout: self.progress_timeout,
            };
            // Never abandon a native constructor on cancellation/timeout. Wait
            // for this same admission, then fence its stream before releasing it.
            let prepare = async {
                if capture.prefix_only {
                    FailoverStream::resume_prefix(&capture.dir, request, saved.end).await
                } else {
                    FailoverStream::resume_local(&capture.dir, request).await
                }
            };
            tokio::pin!(prepare);
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            let stream = loop {
                tokio::select! {
                    biased;
                    _ = tick.tick() => { self.check(); }
                    result = &mut prepare => break result.map_err(|_| RecoveryError::Invalid)?.0,
                }
            };
            let reader = stream.status_reader();
            let complete = if self.check() {
                false
            } else {
                self.stage(RecoveryPhase::Transcribing, None);
                if let Some(job) = self.state().job.as_mut() {
                    job.observed = Some((saved.source, reader.clone()));
                }
                // resume_local freezes input before launch. Its worker drains
                // and completes by itself; finish() would add an unrelated total
                // drain timeout that incorrectly truncates healthy long jobs.
                loop {
                    self.progress(saved.source, &reader);
                    if self.check() {
                        break false;
                    }
                    let status = reader.read();
                    if !status.worker_running {
                        break status.phase
                            == if capture.prefix_only {
                                crate::failover::stream::Phase::PrefixRecovered
                            } else {
                                crate::failover::stream::Phase::Complete
                            };
                    }
                    tick.tick().await;
                }
            };
            self.progress(saved.source, &reader);
            drop(stream); // Fences late output before returning any timeout/cancel result.
            if reader.read().worker_running {
                self.stage(RecoveryPhase::Stopping, None);
            }
            while reader.read().worker_running {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if !complete || self.check() {
                return Err(RecoveryError::Invalid);
            }
        }
        if self.check() {
            return Err(RecoveryError::Invalid);
        }
        if capture.prefix_only {
            let _projection_leases =
                crate::interrupted_capture::projection_leases(&capture.dir, &capture.session)
                    .map_err(|_| RecoveryError::Invalid)?;
            let mut db = self.db();
            if db
                .session_note_meeting(&capture.session)
                .map_err(|_| RecoveryError::Invalid)?
                .as_deref()
                != Some(meeting)
                || db
                    .session_note_clock(&capture.session)
                    .map_err(|_| RecoveryError::Invalid)?
                    .is_some()
            {
                return Err(RecoveryError::Invalid);
            }
            let projected = crate::persist::persist_interrupted(
                &mut db,
                &capture.dir,
                capture.started,
                capture.observed_elapsed,
            )
            .map_err(|_| RecoveryError::Invalid)?;
            if projected != meeting {
                return Err(RecoveryError::Invalid);
            }
            drop(db);
            let refreshed = self.load_with_owner(meeting, held_capture)?;
            self.stage(RecoveryPhase::Partial, Some(refreshed.status.sources));
            return Ok(RecoveryPhase::Partial);
        }
        // Revalidate the saved alias/deletion/clock and atomically project
        // receipts. No native model or inference wait holds this SQL mutex.
        crate::persist::refresh_speech_projection(&mut self.db(), &capture.dir, meeting)
            .map_err(|_| RecoveryError::Invalid)?;
        let refreshed = self.load_with_owner(meeting, held_capture)?;
        if refreshed.status.phase != RecoveryPhase::Complete {
            return Err(RecoveryError::Invalid);
        }
        self.stage(RecoveryPhase::Complete, Some(refreshed.status.sources));
        Ok(RecoveryPhase::Complete)
    }
}
impl SpeechRecoveryControl for SavedSpeechRecovery {
    fn read(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError> {
        self.check();
        let mut recovered_prefix = false;
        {
            let state = self.state();
            if let Some(job) = &state.job
                && job.meeting == meeting
            {
                if job.status.worker_running || job.status.phase != RecoveryPhase::Partial {
                    return Ok(job.status.clone());
                }
                recovered_prefix = true;
            }
        }
        let mut status = self.load(meeting)?.status;
        if recovered_prefix && status.phase == RecoveryPhase::Retained {
            status.phase = RecoveryPhase::Partial;
            status.message=Some("Some saved speech has been recovered. The final recording time is unknown and additional original audio remains pending.".into());
        }
        status.can_retry &= !self
            .state()
            .job
            .as_ref()
            .is_some_and(|j| j.status.worker_running);
        Ok(status)
    }
    fn retry(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError> {
        self.start_job(meeting, false)
    }
    fn cancel(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError> {
        {
            let state = self.state();
            let job = state
                .job
                .as_ref()
                .filter(|j| j.meeting == meeting)
                .ok_or(RecoveryError::NotFound)?;
            state.startup_stop.store(true, Ordering::Release);
            if job.status.worker_running {
                let _ = job
                    .stop
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
            }
        }
        self.read(meeting)
    }
}
struct GuardedFactory {
    inner: Arc<dyn RecognizerFactory>,
    stop: Arc<AtomicU8>,
}
fn not_cancelled(stop: &AtomicU8) -> Result<(), fotw_stt::SttError> {
    if stop.load(Ordering::Acquire) == 0 {
        Ok(())
    } else {
        Err(fotw_stt::SttError::new(
            fotw_stt::SttErrorClass::Server,
            "local",
            "saved recovery publication revoked; audio retained",
        ))
    }
}
impl RecognizerFactory for GuardedFactory {
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn language(&self) -> Option<&str> {
        self.inner.language()
    }
    fn create(&self) -> Result<Box<dyn fotw_stt::Recognizer>, fotw_stt::SttError> {
        not_cancelled(&self.stop)?;
        let inner = self.inner.create()?;
        not_cancelled(&self.stop)?;
        Ok(Box::new(GuardedRecognizer {
            inner,
            stop: self.stop.clone(),
        }))
    }
}
struct GuardedRecognizer {
    inner: Box<dyn fotw_stt::Recognizer>,
    stop: Arc<AtomicU8>,
}
impl fotw_stt::Recognizer for GuardedRecognizer {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<fotw_stt::Recognized>, fotw_stt::SttError> {
        not_cancelled(&self.stop)?;
        let result = self.inner.transcribe(pcm)?;
        not_cancelled(&self.stop)?;
        Ok(result)
    }
}

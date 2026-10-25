//! App-owned local recovery of a saved capture, independent of current ASR policy.
use crate::failover::{
    coverage::{CoverageJournal, LocalPlan},
    stream::{FailoverStream, LocalResumeConfig},
};
use fotw_pipeline::replay::{ReplayReader, ReplaySource};
use fotw_store::Db;
use fotw_stt::{RecognizerFactory, Source};
use fotw_web::speech_recovery::{
    RecoveryError, RecoveryPhase, RecoverySource, RecoveryStatus, SpeechRecoveryControl,
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};
use std::time::{Duration, Instant};

/// Installed factory resolver. Called on an owned worker without a database lock.
pub type ModelResolver =
    dyn Fn(&LocalPlan, &Path) -> Result<Arc<dyn RecognizerFactory>, String> + Send + Sync;

struct Job {
    meeting: String,
    status: RecoveryStatus,
    entered: Duration,
    stop: Arc<AtomicU8>, // 1: explicit cancellation, 2: deadline
}
#[derive(Default)]
struct State {
    job: Option<Job>,
}

/// One saved-audio job at a time; no capture or cloud-credential capability.
#[derive(Clone)]
pub struct SavedSpeechRecovery {
    db: Arc<Mutex<Db>>,
    data_root: PathBuf,
    resolver: Arc<ModelResolver>,
    prepare_timeout: Duration,
    inference_timeout: Duration,
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
}
impl std::fmt::Debug for SavedSpeechRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SavedSpeechRecovery(<redacted>)")
    }
}
struct SavedSource {
    source: Source,
    plan: LocalPlan,
    pending: bool,
    complete: bool,
}
struct SavedCapture {
    dir: PathBuf,
    session: String,
    model_path: PathBuf,
    sources: Vec<SavedSource>,
    status: RecoveryStatus,
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
        inference_timeout: Duration,
    ) -> Self {
        let origin = Instant::now();
        Self {
            db: Arc::new(Mutex::new(db)),
            data_root,
            resolver,
            prepare_timeout,
            inference_timeout,
            state: Arc::new(Mutex::new(State::default())),
            clock: Arc::new(move || origin.elapsed()),
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
        // Release SQL ownership before any filesystem or native work.
        let (session, started, elapsed, model_path) = {
            let mut db = self.db();
            db.meetings()
                .get(meeting)
                .map_err(|_| RecoveryError::NotFound)?;
            let mut query = db.conn().prepare("SELECT session_id,started_at_ms,elapsed_ms FROM session_note_links WHERE meeting_id=?1 LIMIT 2").map_err(|_| RecoveryError::Invalid)?;
            let rows = query
                .query_map([meeting], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|_| RecoveryError::Invalid)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| RecoveryError::Invalid)?;
            if rows.len() != 1 {
                return Err(RecoveryError::NotFound);
            }
            let (session, start, elapsed) = rows.into_iter().next().unwrap();
            let candidate = crate::stt::SttSettings::read(&db)
                .ok()
                .map(|s| s.model_path(&self.data_root))
                .unwrap_or_default();
            (session, start, elapsed, candidate)
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
        // A clean recording's saved clock is authoritative. A raw crash WAL
        // needs its separate capture-clock recovery before inference admission.
        if !wal.is_finalized()
            || wal.manifest.id != session
            || wal.manifest.started_at_ms != started as u64
        {
            return Err(RecoveryError::Invalid);
        }
        let mut sources = Vec::new();
        let mut status = initial(RecoveryPhase::Complete);
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
            let plan = snapshot.fallback.ok_or(RecoveryError::Invalid)?;
            let reader =
                ReplayReader::open(&dir, &session, replay).map_err(|_| RecoveryError::Invalid)?;
            reader.sealed_end().map_err(|_| RecoveryError::Invalid)?;
            let end = reader.saved_end().map_err(|_| RecoveryError::Invalid)?;
            let through = snapshot
                .commits
                .last()
                .map_or(reader.first_sample(), |c| c.through_sample);
            if through > end {
                return Err(RecoveryError::Invalid);
            }
            let complete =
                reader.is_complete().map_err(|_| RecoveryError::Invalid)? && through == end;
            if !complete {
                status.phase = RecoveryPhase::Incomplete;
                status.can_retry = true;
            }
            status.sources.push(RecoverySource {
                source: name.into(),
                model: plan.engine.model.clone(),
                revision: plan.engine.revision.clone(),
                pending_audio_ms: (end - through) / 16,
            });
            sources.push(SavedSource {
                source,
                plan,
                pending: through < end,
                complete,
            });
        }
        if sources.is_empty() {
            return Err(RecoveryError::NotFound);
        }
        Ok(SavedCapture {
            dir,
            session,
            model_path,
            sources,
            status,
        })
    }
    fn stage(&self, phase: RecoveryPhase, sources: Option<Vec<RecoverySource>>) {
        if let Some(job) = self.state().job.as_mut() {
            job.entered = (self.clock)();
            job.status.phase = phase;
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
        let limit = match job.status.phase {
            RecoveryPhase::Preparing => Some(self.prepare_timeout),
            RecoveryPhase::Transcribing => Some(self.inference_timeout),
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
    fn progress(&self, source: Source, reader: &crate::failover::stream::StatusReader) {
        let lag = reader.read().lag_samples / 16;
        let name = match source {
            Source::System => "system",
            Source::Mic => "mic",
        };
        if let Some(job) = self.state().job.as_mut()
            && let Some(source) = job.status.sources.iter_mut().find(|s| s.source == name)
        {
            source.pending_audio_ms = lag;
        }
    }
    fn ended(&self, result: Result<(), RecoveryError>) {
        if let Some(job) = self.state().job.as_mut() {
            job.status.worker_running = false;
            job.status.phase = match job.stop.load(Ordering::Acquire) {
                1 => RecoveryPhase::Cancelled,
                2 => RecoveryPhase::Failed,
                _ if result.is_ok() => RecoveryPhase::Complete,
                _ => RecoveryPhase::Failed,
            };
            job.status.can_retry = job.status.phase != RecoveryPhase::Complete;
            job.status.message = match job.status.phase {
                RecoveryPhase::Complete => Some("Saved speech recovered locally. Existing corrections and recording times preserved.".into()),
                RecoveryPhase::Cancelled => Some("Local recovery cancelled. Audio retained for retry.".into()),
                _ => Some("Local recovery could not finish. Check that the original model revision is installed, then retry; audio retained.".into()),
            };
        }
    }
    async fn recover(&self, meeting: &str) -> Result<(), RecoveryError> {
        let capture = self.load(meeting)?;
        self.stage(RecoveryPhase::Preparing, Some(capture.status.sources));
        for saved in capture.sources {
            if self.check() {
                return Err(RecoveryError::Invalid);
            }
            if saved.complete {
                continue;
            }
            self.stage(RecoveryPhase::Preparing, None);
            let factory = if saved.pending {
                (self.resolver)(&saved.plan, &capture.model_path)
                    .map_err(|_| RecoveryError::Invalid)?
            } else {
                Arc::new(MetadataFactory(saved.plan.clone())) as Arc<dyn RecognizerFactory>
            };
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
                revision: saved.plan.engine.revision,
                stop_timeout: self.inference_timeout,
            };
            // Never abandon a native constructor on cancellation/timeout. Wait
            // for this same admission, then fence its stream before releasing it.
            let prepare = FailoverStream::resume_local(&capture.dir, request);
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
                let finish = stream.finish();
                tokio::pin!(finish);
                loop {
                    tokio::select! {
                        biased;
                        _ = tick.tick() => { self.progress(saved.source, &reader); if self.check() { break false; } }
                        complete = &mut finish => break complete,
                    }
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
        // Revalidate the saved alias/deletion/clock and atomically project
        // receipts. No native model or inference wait holds this SQL mutex.
        crate::persist::refresh_speech_projection(&mut self.db(), &capture.dir, meeting)
            .map_err(|_| RecoveryError::Invalid)?;
        let refreshed = self.load(meeting)?;
        self.stage(RecoveryPhase::Complete, Some(refreshed.status.sources));
        Ok(())
    }
}
impl SpeechRecoveryControl for SavedSpeechRecovery {
    fn read(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError> {
        self.check();
        {
            let state = self.state();
            if let Some(job) = &state.job
                && job.meeting == meeting
            {
                return Ok(job.status.clone());
            }
        }
        let mut status = self.load(meeting)?.status;
        status.can_retry &= !self
            .state()
            .job
            .as_ref()
            .is_some_and(|j| j.status.worker_running);
        Ok(status)
    }
    fn retry(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError> {
        if meeting.is_empty() || meeting.len() > 128 {
            return Err(RecoveryError::NotFound);
        }
        {
            let mut state = self.state();
            if state
                .job
                .as_ref()
                .is_some_and(|job| job.status.worker_running)
            {
                return Err(RecoveryError::Busy);
            }
            state.job = Some(Job {
                meeting: meeting.into(),
                status: RecoveryStatus {
                    worker_running: true,
                    can_retry: false,
                    ..initial(RecoveryPhase::Preparing)
                },
                entered: (self.clock)(),
                stop: Arc::new(AtomicU8::new(0)),
            });
        }
        let owned = self.clone();
        let id = meeting.to_owned();
        let worker = std::thread::Builder::new()
            .name("saved-speech-recovery".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| RecoveryError::Invalid)?;
                    let result = runtime.block_on(owned.recover(&id));
                    // Runtime drop joins any owned blocking cleanup before admission
                    // is released, including a late constructor or file flush.
                    drop(runtime);
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
    fn cancel(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError> {
        {
            let state = self.state();
            let job = state
                .job
                .as_ref()
                .filter(|j| j.meeting == meeting)
                .ok_or(RecoveryError::NotFound)?;
            if job.status.worker_running {
                let _ = job
                    .stop
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
            }
        }
        self.read(meeting)
    }
}
struct MetadataFactory(LocalPlan);
impl RecognizerFactory for MetadataFactory {
    fn model(&self) -> &str {
        &self.0.engine.model
    }
    fn language(&self) -> Option<&str> {
        self.0.language.as_deref()
    }
    fn create(&self) -> Result<Box<dyn fotw_stt::Recognizer>, fotw_stt::SttError> {
        Err(fotw_stt::SttError::new(
            fotw_stt::SttErrorClass::Unsupported,
            "local",
            "completed coverage needs no inference",
        ))
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

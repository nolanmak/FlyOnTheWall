//! The daemon's one cleanup worker: queues a meeting after Stop, runs pending
//! jobs one at a time (including ones a previous daemon left unfinished), and
//! lets the recording tail wait for a result before summarizing.
//!
//! - **Failures back off.** An engine that will not start or a run that
//!   fails is tried again after 30 s, 2 min and 10 min; only then is the job
//!   marked failed, and **Retry cleanup** queues it again.
//! - **Recordings come first.** Between windows a job waits while the
//!   recorder is busy ([`Controller::set_busy`]); its checkpoints make the
//!   pause free.
//! - **Cancel** stops a running job between windows and keeps the live
//!   transcript.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::time::Instant;

use super::engine::PassEngine;
use super::job::{self, Outcome, Plan};
use super::turns::Channel;
use super::windows::{OpusTrack, PcmSource};
use super::{Queued, queue, track_path};

/// Builds the engine for a run: the speech helper in production.
pub type EngineFactory =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn PassEngine>, String>> + Send + Sync>;

/// Opens the library: [`crate::open_library`] in production.
pub type OpenLibrary = Arc<dyn Fn() -> Result<fotw_store::Db, String> + Send + Sync>;

/// Waits before trying a failed job again; after the last, it fails.
pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];
/// How often a waiting tail looks at the job when no completion woke it
/// (each look opens the library: a keychain read and SQLCipher).
const POLL: Duration = Duration::from_secs(30);
/// How often an idle worker re-reads the queue without being woken.
const IDLE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct Attempts {
    failures: usize,
    next_at: Instant,
}

/// The worker.
pub struct Controller {
    data_root: PathBuf,
    open: OpenLibrary,
    factory: EngineFactory,
    /// The engines' provenance, for new jobs' plans, known without starting
    /// the helper.
    provenance: serde_json::Value,
    wake: tokio::sync::Notify,
    /// Fired whenever a job stops being pending.
    settled: tokio::sync::Notify,
    stop: AtomicBool,
    attempts: Mutex<HashMap<String, Attempts>>,
    /// The meeting being run and its stop flag.
    running: Mutex<Option<(String, Arc<AtomicBool>)>>,
    busy: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
    on_done: Box<dyn Fn(&str) + Send + Sync>,
}

impl Controller {
    /// A worker over the library at `data_root`. Call [`Controller::start`]
    /// on the daemon's runtime.
    pub fn new(
        data_root: &Path,
        open: OpenLibrary,
        factory: EngineFactory,
        provenance: serde_json::Value,
        on_done: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            data_root: data_root.to_owned(),
            open,
            factory,
            provenance,
            wake: tokio::sync::Notify::new(),
            settled: tokio::sync::Notify::new(),
            stop: AtomicBool::new(false),
            attempts: Mutex::default(),
            running: Mutex::default(),
            busy: OnceLock::new(),
            on_done,
        })
    }

    /// Jobs wait between windows while `busy()` is true: the recorder is
    /// capturing, and live transcription has the helper.
    pub fn set_busy(&self, busy: Box<dyn Fn() -> bool + Send + Sync>) {
        let _ = self.busy.set(busy);
    }

    fn is_busy(&self) -> bool {
        self.busy.get().is_some_and(|busy| busy())
    }

    /// The plan new jobs get.
    #[must_use]
    pub fn plan(&self) -> Plan {
        Plan {
            version: 1,
            window_ms: super::windows::WINDOW_MS,
            overlap_ms: super::windows::OVERLAP_MS,
            engines: self.provenance.clone(),
        }
    }

    fn attempts(&self) -> std::sync::MutexGuard<'_, HashMap<String, Attempts>> {
        self.attempts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn settle(&self, meeting: &str) {
        self.attempts().remove(meeting);
        (self.on_done)(meeting);
        self.settled.notify_waiters();
    }

    /// Run pending jobs until [`Controller::shutdown`], resuming any a
    /// previous daemon left.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            while !this.stop.load(Ordering::SeqCst) {
                let pending = (this.open)()
                    .and_then(|db| db.post_pass_pending().map_err(|e| e.to_string()))
                    .unwrap_or_default();
                let now = Instant::now();
                let due = |m: &String| this.attempts().get(m).is_none_or(|a| a.next_at <= now);
                let mut ran = false;
                for meeting in pending.iter().filter(|m| due(m)) {
                    if this.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    ran = true;
                    this.run_one(meeting).await;
                }
                if !ran {
                    // The next retry, a new job, or a periodic re-read.
                    let next = pending
                        .iter()
                        .filter_map(|m| this.attempts().get(m).map(|a| a.next_at))
                        .min()
                        .unwrap_or(now + IDLE)
                        .min(now + IDLE);
                    let _ = tokio::time::timeout_at(next, this.wake.notified()).await;
                }
            }
        })
    }

    /// Count a failure: schedule the next try, or fail the job after the last.
    fn failed(&self, meeting: &str, error: &str) {
        let mut attempts = self.attempts();
        let entry = attempts.entry(meeting.to_owned()).or_insert(Attempts {
            failures: 0,
            next_at: Instant::now(),
        });
        entry.failures += 1;
        if let Some(wait) = BACKOFF.get(entry.failures - 1) {
            entry.next_at = Instant::now() + *wait;
            crate::journal::record(&format!(
                "cleanup  : meeting {meeting} will retry in {}s: {error}",
                wait.as_secs()
            ));
            return;
        }
        drop(attempts);
        if let Ok(mut db) = (self.open)() {
            let _ = db.post_pass_fail(meeting, error);
        }
        crate::journal::record(&format!("cleanup  : meeting {meeting} failed: {error}"));
        self.settle(meeting);
    }

    /// Whether the job is still pending (a cancel or another process may
    /// have settled it).
    fn still_pending(&self, meeting: &str) -> bool {
        (self.open)()
            .ok()
            .and_then(|db| db.post_pass_job(meeting).ok().flatten())
            .is_some_and(|job| job.state == fotw_store::PostPassState::Pending)
    }

    async fn run_one(&self, meeting: &str) {
        crate::journal::record(&format!("cleanup  : meeting {meeting} starting"));
        let engine = match (self.factory)().await {
            Ok(engine) => engine,
            // Missing models, a helper that will not start: often fixed by
            // the person (an install) or by time, so it backs off too.
            Err(e) => return self.failed(meeting, &e),
        };
        let db = match (self.open)() {
            Ok(db) => Mutex::new(db),
            Err(e) => return self.failed(meeting, &format!("could not open the library: {e}")),
        };
        let paths = {
            let db = db.lock().unwrap_or_else(|e| e.into_inner());
            [Channel::Mic, Channel::System].map(|c| track_path(&db, &self.data_root, meeting, c))
        };
        let open = move |channel: Channel| -> Result<Option<Box<dyn PcmSource>>, String> {
            let path = match channel {
                Channel::Mic => &paths[0],
                Channel::System => &paths[1],
            };
            match path.as_ref().map_err(Clone::clone)? {
                Some(path) => Ok(Some(Box::new(OpusTrack::open(path)?) as Box<dyn PcmSource>)),
                None => Ok(None),
            }
        };
        let stop = Arc::new(AtomicBool::new(self.stop.load(Ordering::SeqCst)));
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((meeting.to_owned(), Arc::clone(&stop)));
        let pause = || self.is_busy();
        let result = job::run(&db, meeting, engine.as_ref(), &open, &stop, &pause).await;
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
        match result {
            Ok(Outcome::Done(_)) => {
                crate::journal::record(&format!("cleanup  : meeting {meeting} finished"));
                self.settle(meeting);
            }
            Ok(Outcome::Stopped) => {
                if !self.still_pending(meeting) {
                    self.settle(meeting);
                }
            }
            // A cancel turns the job's next checkpoint into an error.
            Err(_) if !self.still_pending(meeting) => self.settle(meeting),
            Err(e) => self.failed(meeting, &e),
        }
    }

    /// Queue `meeting_id` after Stop, if its settings and audio allow, and
    /// wake the worker.
    pub fn after_stop(&self, meeting_id: &str) -> Queued {
        let queued = (self.open)()
            .and_then(|mut db| queue(&mut db, meeting_id, &self.plan()))
            .unwrap_or_else(|e| {
                crate::journal::record(&format!("cleanup  : meeting {meeting_id} not queued: {e}"));
                Queued::Off
            });
        if queued == Queued::Yes {
            self.wake.notify_one();
        }
        queued
    }

    /// Queue a failed job again, now. Returns whether it was failed.
    pub fn retry(&self, meeting_id: &str) -> bool {
        let retried = (self.open)()
            .and_then(|mut db| db.post_pass_retry(meeting_id).map_err(|e| e.to_string()))
            .unwrap_or(false);
        if retried {
            self.attempts().remove(meeting_id);
            self.wake.notify_one();
        }
        retried
    }

    /// Cancel a pending job; a running one stops before its next window.
    /// The live transcript stays. Returns whether one was pending.
    pub fn cancel(&self, meeting_id: &str) -> bool {
        let cancelled = (self.open)()
            .and_then(|mut db| db.post_pass_cancel(meeting_id).map_err(|e| e.to_string()))
            .unwrap_or(false);
        if cancelled {
            if let Some((running, stop)) = &*self.running.lock().unwrap_or_else(|e| e.into_inner())
                && running == meeting_id
            {
                stop.store(true, Ordering::SeqCst);
            } else {
                self.settle(meeting_id);
            }
        }
        cancelled
    }

    /// Wait until `meeting_id`'s job is no longer pending, or for at most
    /// `limit`. Returns whether it finished in time. Woken by the worker when
    /// a job settles, so the library is opened rarely.
    pub async fn wait(&self, meeting_id: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if !self.still_pending(meeting_id) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let _ = tokio::time::timeout_at((now + POLL).min(deadline), settled).await;
        }
    }

    /// Stop between windows; the job resumes at the next start.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some((_, stop)) = &*self.running.lock().unwrap_or_else(|e| e.into_inner()) {
            stop.store(true, Ordering::SeqCst);
        }
        self.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use fotw_store::{Db, DbKey, NewMeeting, NewRecording, PostPassState};

    use super::super::engine::fake::FakeEngine;
    use super::*;

    /// A file-backed library each call reopens, like the daemon's.
    fn library(dir: &Path) -> OpenLibrary {
        let path = dir.join("db.sqlite3");
        Arc::new(move || Db::open(&path, &DbKey::from_bytes([0x77; 32])).map_err(|e| e.to_string()))
    }

    /// A meeting with one archived Opus system track of fake words.
    fn meeting(dir: &Path, open: &OpenLibrary) -> String {
        let mut db = open().unwrap();
        let id = db
            .meetings()
            .create(NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        db.meetings()
            .create_transcript(&id, "fotw-speechd", "parakeet-unified", true)
            .unwrap();
        let rel = format!("media/{id}/system.opus");
        std::fs::create_dir_all(dir.join(format!("media/{id}"))).unwrap();
        let mut w = fotw_pipeline::opus::OpusOggWriter::create(dir.join(&rel), 16_000).unwrap();
        let tone: Vec<f32> = (0..16_000 * 40)
            .map(|i| 0.3 * (i as f32 * 0.05).sin())
            .collect();
        w.push_f32(&tone).unwrap();
        w.finish().unwrap();
        db.upsert_recording(
            &id,
            &NewRecording {
                channel: "system".into(),
                rel_path: rel,
                bytes: 1,
                duration_ms: 40_000,
                sample_rate_hz: 16_000,
            },
        )
        .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_queued_meeting_is_cleaned_from_its_archived_opus_and_the_tail_sees_it_finish() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let engine = Arc::new(FakeEngine::default());
        let provenance = engine.provenance();
        let calls = Arc::clone(&engine.transcribed);
        let done = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&done);
        let factory: EngineFactory = Arc::new(move || {
            let engine = Arc::clone(&engine) as Arc<dyn PassEngine>;
            Box::pin(async move { Ok(engine) })
        });
        let controller = Controller::new(
            dir.path(),
            open.clone(),
            factory,
            provenance,
            Box::new(move |m| seen.lock().unwrap().push(m.to_owned())),
        );
        let worker = controller.start();
        assert_eq!(controller.after_stop(&id), Queued::Yes);
        assert!(controller.wait(&id, Duration::from_secs(30)).await);
        let job = open().unwrap().post_pass_job(&id).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Done, "{:?}", job.detail);
        // Two windows of a 40 s track, both heard as speech.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(*done.lock().unwrap(), std::slice::from_ref(&id));
        controller.shutdown();
        worker.await.unwrap();
    }

    fn controller(
        dir: &Path,
        open: &OpenLibrary,
        factory: EngineFactory,
    ) -> (Arc<Controller>, Arc<Mutex<Vec<String>>>) {
        let done = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&done);
        let c = Controller::new(
            dir,
            open.clone(),
            factory,
            FakeEngine::default().provenance(),
            Box::new(move |m| seen.lock().unwrap().push(m.to_owned())),
        );
        (c, done)
    }

    /// A factory that fails `failures` times, recording when it was asked,
    /// then serves the fake engine.
    fn flaky(failures: usize, calls: Arc<Mutex<Vec<Instant>>>) -> EngineFactory {
        Arc::new(move || {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                let mut calls = calls.lock().unwrap();
                calls.push(Instant::now());
                if calls.len() <= failures {
                    Err("Parakeet Unified is not installed".to_owned())
                } else {
                    Ok(Arc::new(FakeEngine::default()) as Arc<dyn PassEngine>)
                }
            })
        })
    }

    fn gaps(calls: &[Instant]) -> Vec<u64> {
        calls.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_that_will_not_start_is_retried_with_backoff_then_the_job_fails() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let live = open()
            .unwrap()
            .meetings()
            .primary_transcript_id(&id)
            .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (controller, done) =
            controller(dir.path(), &open, flaky(usize::MAX, Arc::clone(&calls)));
        let worker = controller.start();
        controller.after_stop(&id);
        assert!(controller.wait(&id, Duration::from_secs(3_600)).await);
        let mut db = open().unwrap();
        let job = db.post_pass_job(&id).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Failed);
        assert!(job.detail.unwrap().contains("not installed"));
        assert_eq!(db.meetings().primary_transcript_id(&id).unwrap(), live);
        assert_eq!(gaps(&calls.lock().unwrap()), [30, 120, 600]);
        assert_eq!(*done.lock().unwrap(), std::slice::from_ref(&id));
        controller.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_that_clears_up_lets_the_job_finish() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (controller, _) = controller(dir.path(), &open, flaky(2, Arc::clone(&calls)));
        let worker = controller.start();
        controller.after_stop(&id);
        assert!(controller.wait(&id, Duration::from_secs(3_600)).await);
        assert_eq!(
            open().unwrap().post_pass_job(&id).unwrap().unwrap().state,
            PostPassState::Done
        );
        assert_eq!(gaps(&calls.lock().unwrap()), [30, 120]);
        controller.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retry_cleanup_runs_a_failed_job_again() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let calls = Arc::new(Mutex::new(Vec::new()));
        // Fails all four tries, then works when retried.
        let (controller, _) = controller(dir.path(), &open, flaky(4, Arc::clone(&calls)));
        let worker = controller.start();
        controller.after_stop(&id);
        assert!(controller.wait(&id, Duration::from_secs(3_600)).await);
        assert_eq!(
            open().unwrap().post_pass_job(&id).unwrap().unwrap().state,
            PostPassState::Failed
        );
        assert!(controller.retry(&id));
        assert!(controller.wait(&id, Duration::from_secs(60)).await);
        assert_eq!(
            open().unwrap().post_pass_job(&id).unwrap().unwrap().state,
            PostPassState::Done
        );
        controller.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_stops_a_running_job_and_keeps_the_live_version() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let live = open()
            .unwrap()
            .meetings()
            .primary_transcript_id(&id)
            .unwrap();
        // Hangs in its first window, like a long transcription.
        let engine = Arc::new(FakeEngine {
            hang_on: Some(1),
            ..FakeEngine::default()
        });
        let started = Arc::clone(&engine.started);
        let factory: EngineFactory = Arc::new(move || {
            let engine = Arc::clone(&engine) as Arc<dyn PassEngine>;
            Box::pin(async move { Ok(engine) })
        });
        let (controller, done) = controller(dir.path(), &open, factory);
        let _worker = controller.start();
        controller.after_stop(&id);
        while started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(controller.cancel(&id));
        let mut db = open().unwrap();
        let job = db.post_pass_job(&id).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Failed);
        assert_eq!(job.detail.as_deref(), Some(fotw_store::POST_PASS_CANCELLED));
        assert_eq!(db.meetings().primary_transcript_id(&id).unwrap(), live);
        assert!(!controller.cancel(&id), "nothing left to cancel");
        let _ = done;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cleanup_waits_while_the_recorder_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let engine = Arc::new(FakeEngine::default());
        let started = Arc::clone(&engine.started);
        let factory: EngineFactory = Arc::new(move || {
            let engine = Arc::clone(&engine) as Arc<dyn PassEngine>;
            Box::pin(async move { Ok(engine) })
        });
        let (controller, _) = controller(dir.path(), &open, factory);
        // A stand-in for the recorder: capturing until told otherwise.
        let recording = Arc::new(AtomicBool::new(true));
        let busy = Arc::clone(&recording);
        controller.set_busy(Box::new(move || busy.load(Ordering::SeqCst)));
        let worker = controller.start();
        controller.after_stop(&id);
        assert!(!controller.wait(&id, Duration::from_millis(1_500)).await);
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "no window while recording"
        );
        recording.store(false, Ordering::SeqCst);
        assert!(controller.wait(&id, Duration::from_secs(30)).await);
        assert_eq!(
            open().unwrap().post_pass_job(&id).unwrap().unwrap().state,
            PostPassState::Done
        );
        controller.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_tail_opens_the_library_rarely() {
        let dir = tempfile::tempdir().unwrap();
        let inner = library(dir.path());
        let id = meeting(dir.path(), &inner);
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&opens);
        let open: OpenLibrary = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            inner()
        });
        let (controller, _) = controller(
            dir.path(),
            &open,
            Arc::new(|| Box::pin(std::future::pending())),
        );
        assert_eq!(controller.after_stop(&id), Queued::Yes);
        let before = opens.load(Ordering::SeqCst);
        // Fifteen minutes with nothing finishing: one look per 30 s.
        assert!(!controller.wait(&id, Duration::from_secs(15 * 60)).await);
        let looks = opens.load(Ordering::SeqCst) - before;
        assert!(looks <= 32, "{looks} library opens");
    }

    #[tokio::test]
    async fn the_tail_stops_waiting_after_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let (controller, _) = controller(
            dir.path(),
            &open,
            Arc::new(|| Box::pin(std::future::pending())),
        );
        assert_eq!(controller.after_stop(&id), Queued::Yes);
        // No worker runs it, as when the helper never answers.
        let started = std::time::Instant::now();
        assert!(!controller.wait(&id, Duration::from_millis(300)).await);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

//! The daemon's one cleanup worker: queues a meeting after Stop, runs pending
//! jobs one at a time (including ones a previous daemon left unfinished), and
//! lets the recording tail wait for a result before summarizing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;

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

/// A job's engine failures in one daemon run before it is marked failed.
pub const ATTEMPTS: u32 = 3;
/// How often a waiting tail looks at the job.
const POLL: Duration = Duration::from_secs(2);

/// The worker.
pub struct Controller {
    data_root: PathBuf,
    open: OpenLibrary,
    factory: EngineFactory,
    /// The engines' provenance, for new jobs' plans, known without starting
    /// the helper.
    provenance: serde_json::Value,
    wake: tokio::sync::Notify,
    stop: AtomicBool,
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
            stop: AtomicBool::new(false),
            on_done,
        })
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

    /// Run pending jobs until [`Controller::shutdown`], resuming any a
    /// previous daemon left.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut attempts: std::collections::HashMap<String, u32> = Default::default();
            while !this.stop.load(Ordering::SeqCst) {
                let pending = (this.open)()
                    .and_then(|db| db.post_pass_pending().map_err(|e| e.to_string()))
                    .unwrap_or_default();
                let mut ran = false;
                for meeting in pending {
                    if this.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if attempts.get(&meeting).copied().unwrap_or(0) >= ATTEMPTS {
                        continue;
                    }
                    ran = true;
                    this.run_one(&meeting, &mut attempts).await;
                }
                if !ran {
                    // Woken by a new job; the timeout only re-reads the queue.
                    let _ =
                        tokio::time::timeout(Duration::from_secs(60), this.wake.notified()).await;
                }
            }
        })
    }

    async fn run_one(&self, meeting: &str, attempts: &mut std::collections::HashMap<String, u32>) {
        crate::journal::record(&format!("cleanup  : meeting {meeting} starting"));
        let fail = |detail: &str| {
            if let Ok(mut db) = (self.open)() {
                let _ = db.post_pass_fail(meeting, detail);
            }
            crate::journal::record(&format!("cleanup  : meeting {meeting} failed: {detail}"));
            (self.on_done)(meeting);
        };
        let engine = match (self.factory)().await {
            Ok(engine) => engine,
            // Missing models or no helper in this build: nothing to retry.
            Err(e) => return fail(&e),
        };
        let db = match (self.open)() {
            Ok(db) => Mutex::new(db),
            Err(e) => {
                *attempts.entry(meeting.to_owned()).or_default() += 1;
                crate::journal::record(&format!(
                    "cleanup  : meeting {meeting} could not open the library: {e}"
                ));
                return;
            }
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
        match job::run(&db, meeting, engine.as_ref(), &open, &self.stop).await {
            Ok(Outcome::Done(_)) => {
                crate::journal::record(&format!("cleanup  : meeting {meeting} finished"));
                (self.on_done)(meeting);
            }
            Ok(Outcome::Stopped) => {}
            Err(e) => {
                let tries = attempts.entry(meeting.to_owned()).or_default();
                *tries += 1;
                if *tries >= ATTEMPTS {
                    fail(&e);
                } else {
                    crate::journal::record(&format!(
                        "cleanup  : meeting {meeting} will retry: {e}"
                    ));
                }
            }
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

    /// Wait until `meeting_id`'s job is no longer pending, or for at most
    /// `limit`. Returns whether it finished in time.
    pub async fn wait(&self, meeting_id: &str, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let pending = (self.open)()
                .ok()
                .and_then(|db| db.post_pass_job(meeting_id).ok().flatten())
                .is_some_and(|job| job.state == fotw_store::PostPassState::Pending);
            if !pending {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL.min(deadline - tokio::time::Instant::now())).await;
        }
    }

    /// Stop between windows; the job resumes at the next start.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
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
        assert_eq!(*done.lock().unwrap(), [id.clone()]);
        controller.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_model_fails_the_job_plainly_and_keeps_the_live_version() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let live = open()
            .unwrap()
            .meetings()
            .primary_transcript_id(&id)
            .unwrap();
        let factory: EngineFactory =
            Arc::new(|| Box::pin(async { Err("Parakeet Unified is not installed".to_owned()) }));
        let controller = Controller::new(
            dir.path(),
            open.clone(),
            factory,
            serde_json::json!({}),
            Box::new(|_| {}),
        );
        let worker = controller.start();
        controller.after_stop(&id);
        assert!(controller.wait(&id, Duration::from_secs(30)).await);
        let mut db = open().unwrap();
        let job = db.post_pass_job(&id).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Failed);
        assert!(job.detail.unwrap().contains("not installed"));
        assert_eq!(db.meetings().primary_transcript_id(&id).unwrap(), live);
        controller.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn the_tail_stops_waiting_after_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let open = library(dir.path());
        let id = meeting(dir.path(), &open);
        let controller = Controller::new(
            dir.path(),
            open.clone(),
            Arc::new(|| Box::pin(std::future::pending())),
            serde_json::json!({}),
            Box::new(|_| {}),
        );
        assert_eq!(controller.after_stop(&id), Queued::Yes);
        // No worker runs it, as when the helper never answers.
        let started = std::time::Instant::now();
        assert!(!controller.wait(&id, Duration::from_millis(300)).await);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

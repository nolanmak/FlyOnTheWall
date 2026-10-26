//! Local-only sources have no cloud configuration and retain exact restart input.
use fotw_pipeline::replay::{ReplayReader, ReplaySource, ReplayWriter};
use fotw_stt::{
    LocalStreamConfig, Recognized, Recognizer, RecognizerFactory, Source, SttError, SttErrorClass,
};
use fotwd::failover::{
    coverage::CoverageJournal,
    stream::{FailoverStream, LocalResumeConfig, LocalSourceConfig, Phase},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

struct Factory {
    fail: bool,
    block_drop: Option<Arc<(Mutex<bool>, Condvar)>>,
    entered: Arc<AtomicBool>,
}
struct Model {
    fail: bool,
    block_drop: Option<Arc<(Mutex<bool>, Condvar)>>,
    entered: Arc<AtomicBool>,
}
impl RecognizerFactory for Factory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(Model {
            fail: self.fail,
            block_drop: self.block_drop.clone(),
            entered: self.entered.clone(),
        }))
    }
}
impl Recognizer for Model {
    fn transcribe(&mut self, _: &[f32]) -> Result<Vec<Recognized>, SttError> {
        if self.fail {
            Err(SttError::new(
                SttErrorClass::Server,
                "fixture",
                "synthetic inference failure",
            ))
        } else {
            Ok(vec![Recognized {
                start_ms: 0,
                end_ms: 100,
                text: "local recorded words".into(),
            }])
        }
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        if let Some(release) = &self.block_drop {
            self.entered.store(true, Ordering::Release);
            let mut ready = release.0.lock().unwrap();
            while !*ready {
                ready = release.1.wait(ready).unwrap();
            }
        }
    }
}
struct Release(Arc<(Mutex<bool>, Condvar)>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
fn factory(fail: bool) -> Arc<dyn RecognizerFactory> {
    Arc::new(Factory {
        fail,
        block_drop: None,
        entered: Arc::new(AtomicBool::new(false)),
    })
}
fn pcm() -> Vec<i16> {
    (0..32000)
        .map(|n| if (n / 480) % 2 == 0 { 1000 } else { 8000 })
        .collect()
}
fn config(local: Arc<dyn RecognizerFactory>) -> LocalSourceConfig {
    let mut config = LocalSourceConfig::new(
        LocalStreamConfig::new("fixture", Source::Mic).with_session_offset_ms(137),
        local,
    );
    config.revision = Some("fixture-v1".into());
    config.stop_timeout = Duration::from_secs(2);
    config
}
fn resume(local: Arc<dyn RecognizerFactory>) -> LocalResumeConfig {
    LocalResumeConfig {
        session_id: "fixture".into(),
        source: Source::Mic,
        local,
        revision: Some("fixture-v1".into()),
        stop_timeout: Duration::from_secs(2),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_local_inference_can_restart_exact_input_with_no_cloud_plan() {
    let dir = tempfile::tempdir().unwrap();
    let (stream, _) = FailoverStream::open_local(dir.path(), config(factory(true)))
        .await
        .unwrap();
    let audio = pcm();
    stream.write(&audio).unwrap();
    assert!(!stream.finish().await);
    assert_eq!(stream.status().phase, Phase::Incomplete);
    let status = stream.status_reader();
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(2);
    while status.read().worker_running {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let mut reader = ReplayReader::open(dir.path(), "fixture", ReplaySource::Mic).unwrap();
    let mut retained = reader.read_from(137 * 16, 16000).unwrap();
    retained.extend(reader.read_from(137 * 16 + 16000, 16000).unwrap());
    assert_eq!(retained, audio);
    let saved = CoverageJournal::snapshot(dir.path(), "fixture", Source::Mic).unwrap();
    assert_eq!(saved.epochs[0].engine.provider, fotw_stt::local::PROVIDER);
    assert_eq!(saved.epochs[0].reason, None);
    assert_eq!(
        saved.fallback.unwrap().engine.revision.as_deref(),
        Some("fixture-v1")
    );
    let (restarted, _) = FailoverStream::resume_local(dir.path(), resume(factory(false)))
        .await
        .unwrap();
    assert!(restarted.finish().await, "{:?}", restarted.status());
    assert_eq!(
        restarted.status().committed_sample,
        137 * 16 + audio.len() as u64
    );
    assert_eq!(restarted.status().lag_samples, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_data_releases_file_leases_while_native_cleanup_still_excludes_restart() {
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let _release = Release(release.clone());
    let entered = Arc::new(AtomicBool::new(false));
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Factory {
        fail: false,
        block_drop: Some(release.clone()),
        entered: entered.clone(),
    });
    let (stream, _) = FailoverStream::open_local(dir.path(), config(model))
        .await
        .unwrap();
    stream.write(&pcm()).unwrap();
    assert!(stream.finish().await);
    let status = stream.status_reader();
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(status.read().worker_running);
    assert_eq!(status.read().phase, Phase::Complete);
    let prefix = ReplayWriter::resume_prefix(dir.path(), "fixture", ReplaySource::Mic).unwrap();
    let journal = CoverageJournal::open(dir.path(), "fixture", Source::Mic).unwrap();
    drop(journal);
    drop(prefix);
    assert!(
        FailoverStream::resume_local(dir.path(), resume(factory(false)))
            .await
            .is_err(),
        "native source lease must outlive data publication"
    );
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    while status.read().worker_running {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let (restarted, _) = FailoverStream::resume_local(dir.path(), resume(factory(false)))
        .await
        .unwrap();
    assert!(restarted.finish().await);
}

struct Initializing {
    release: Arc<(Mutex<bool>, Condvar)>,
    entered: Arc<AtomicBool>,
    inferred: Arc<AtomicBool>,
}
struct Initialized(Arc<AtomicBool>);
impl RecognizerFactory for Initializing {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        self.entered.store(true, Ordering::Release);
        let mut ready = self.release.0.lock().unwrap();
        while !*ready {
            ready = self.release.1.wait(ready).unwrap();
        }
        Ok(Box::new(Initialized(self.inferred.clone())))
    }
}
impl Recognizer for Initialized {
    fn transcribe(&mut self, _: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.0.store(true, Ordering::Release);
        Ok(vec![])
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_local_initialization_keeps_input_and_cannot_infer_after_the_stop_deadline() {
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let _release = Release(release.clone());
    let entered = Arc::new(AtomicBool::new(false));
    let inferred = Arc::new(AtomicBool::new(false));
    let local = Arc::new(Initializing {
        release: release.clone(),
        entered: entered.clone(),
        inferred: inferred.clone(),
    });
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(local);
    cfg.stop_timeout = Duration::from_millis(30);
    let (stream, _) = tokio::time::timeout(
        Duration::from_secs(1),
        FailoverStream::open_local(dir.path(), cfg),
    )
    .await
    .unwrap()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let audio = pcm();
    stream.write(&audio).unwrap();
    let mut reader = ReplayReader::open(dir.path(), "fixture", ReplaySource::Mic).unwrap();
    assert_eq!(reader.saved_end().unwrap(), 137 * 16 + audio.len() as u64);
    assert!(
        !tokio::time::timeout(Duration::from_secs(1), stream.finish())
            .await
            .unwrap()
    );
    assert!(stream.status().worker_running);
    let status = stream.status_reader();
    drop(stream);
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    while status.read().worker_running {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(!inferred.load(Ordering::Acquire));
    assert_eq!(reader.read_from(137 * 16, 16000).unwrap(), audio[..16000]);
    assert_eq!(
        reader.read_from(137 * 16 + 16000, 16000).unwrap(),
        audio[16000..]
    );
}

use super::{FailoverStream, LocalSourceConfig};
use fotw_stt::{LocalStreamConfig, Recognized, Recognizer, RecognizerFactory, Source, SttError};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) const TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Release(pub(super) Arc<(Mutex<bool>, Condvar)>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
/// Speech as the endpointer sees it (`fotw-stt`'s `local.rs` tests): a square
/// wave whose level swings 20 dB every 120 ms, then digital silence.
fn utterance(speech_ms: usize, quiet_ms: usize) -> Vec<i16> {
    let speech = (0..speech_ms * 16).map(|i| {
        let amp: i16 = if (i / 1_920) % 2 == 0 { 8_000 } else { 800 };
        if (i / 20) % 2 == 0 { amp } else { -amp }
    });
    speech
        .chain(std::iter::repeat_n(0, quiet_ms * 16))
        .collect()
}

/// A model that holds its first utterance until released.
struct FirstHeld {
    release: Arc<(Mutex<bool>, Condvar)>,
    entered: Arc<AtomicBool>,
}
struct FirstHeldModel {
    release: Arc<(Mutex<bool>, Condvar)>,
    entered: Arc<AtomicBool>,
}
impl RecognizerFactory for FirstHeld {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(FirstHeldModel {
            release: self.release.clone(),
            entered: self.entered.clone(),
        }))
    }
}
impl Recognizer for FirstHeldModel {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.entered.store(true, Ordering::Release);
        let mut ready = self.release.0.lock().unwrap();
        while !*ready {
            ready = self.release.1.wait(ready).unwrap();
        }
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: pcm.len() as u64 / 16,
            text: "local recorded words".into(),
        }])
    }
}

/// Open a local source whose model holds the first utterance, write that
/// utterance, wait until the model has it, then write a second one.
pub(super) async fn held_after_more_input(
    dir: &std::path::Path,
) -> (FailoverStream, Release, Vec<i16>) {
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_on_drop = Release(release.clone());
    let entered = Arc::new(AtomicBool::new(false));
    let local = Arc::new(FirstHeld {
        release: release.clone(),
        entered: entered.clone(),
    });
    let mut cfg = LocalSourceConfig::new(
        LocalStreamConfig::new("fixture", Source::Mic).with_session_offset_ms(137),
        local,
    );
    cfg.revision = Some("fixture-v1".into());
    cfg.stop_timeout = TIMEOUT;
    let (stream, _) = FailoverStream::open_local(dir, cfg).await.unwrap();
    // One utterance, cut at its pause: the model starts on it and is held.
    let mut audio = utterance(500, 900);
    stream.write(&audio).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the model never started");
        tokio::task::yield_now().await;
    }
    // More audio arrives while the model works.
    let second = utterance(500, 900);
    stream.write(&second).unwrap();
    audio.extend(second);
    (stream, release_on_drop, audio)
}

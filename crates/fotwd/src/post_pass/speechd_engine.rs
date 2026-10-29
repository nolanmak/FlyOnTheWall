//! The cleanup's engines in production: Silero VAD in-process (the same
//! pinned ggml detector that gates live local transcription, #242), and
//! Parakeet Unified with word timings plus the diarizer in the daemon's
//! shared speech helper (#243, docs/SPEECHD_PROTOCOL.md).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use fotw_speechd_client::message::StreamOptions;
use fotw_speechd_client::{
    DIARIZATION_MAX_UNACKED_MS, SpeechStream, SpeechdClient, StreamConfig, StreamEvent,
};
use fotw_stt::vad::{GateConfig, SpeechDetector, speech_regions};
use futures_util::future::BoxFuture;

use super::bleed::BleedParams;
use super::engine::{Diarization, PassEngine};
use super::merge::TimedWord;
use super::turns::SpeakerSpan;
use super::windows::RATE;
use crate::speechd::{DIARIZER, PARAKEET};

/// The diarizer's engine id in the helper.
pub const DIARIZER_ENGINE: &str = "diarizer";
/// The diarizer's speaker threshold for meetings. FluidAudio's default
/// (0.84) merged two of three remote speakers on AMI ES2004c; swept over
/// 0.84/0.7/0.6/0.5/0.4 on the three AMI meetings, 0.6 had the lowest DER on
/// ES2004c and the lowest total; on IS1009c 0.7 was 0.2 points lower, on
/// TS3003c they tied. Chosen on the acceptance meetings themselves, so the
/// numbers are not held-out (docs/POST_MEETING_CLEANUP.md).
pub const SPEAKER_THRESHOLD: Option<f32> = Some(0.6);
/// PCM per audio frame to the helper: 5 s, well under its 1 MiB frame.
const FRAME: usize = 16_000 * 5;

/// What the cleanup runs, by pinned revision. Known without starting the
/// helper, so a job queued now and resumed after an upgrade can tell.
#[must_use]
pub fn provenance() -> serde_json::Value {
    serde_json::json!({
        "asr": format!("{}@{}", PARAKEET.id, PARAKEET.revision),
        "vad": format!("silero-v5.1.2@{}", crate::stt::VAD_MODEL.sha256),
        "diarizer": format!("{}@{}", DIARIZER.id, DIARIZER.revision),
        "speaker_threshold": SPEAKER_THRESHOLD,
    })
}

/// The plan new cleanup jobs get in this build: [`provenance`] and the
/// cross-track bleed suppression [`HelperEngine`] runs.
#[must_use]
pub fn plan() -> super::job::Plan {
    super::job::Plan::new(provenance(), Some(BleedParams::default()))
}

/// Parakeet and the diarizer in the helper, Silero beside them.
pub struct HelperEngine {
    client: SpeechdClient,
    asr: &'static str,
    diarizer_engine: &'static str,
    detector: Arc<Mutex<Box<dyn SpeechDetector>>>,
    /// The diarizer is installed; it is loaded only once Parakeet is done.
    diarizer: bool,
    /// This engine loaded Parakeet (a recording had not), so it unloads it.
    owns_asr: AtomicBool,
    /// This engine loaded the diarizer, so it unloads it.
    owns_diarizer: AtomicBool,
    speaker_threshold: Option<f32>,
    /// Cross-track bleed suppression before the engines hear a track.
    bleed: Option<BleedParams>,
    /// Ask Parakeet for word timings (always, outside tests).
    request_timings: bool,
    /// Windows whose word times were estimated, because the helper returned
    /// text without them.
    estimated: std::sync::atomic::AtomicUsize,
}

impl HelperEngine {
    /// Load and warm Parakeet in the daemon's shared helper and load the
    /// speech detector. The diarizer, when installed, is loaded only after
    /// Parakeet is released ([`PassEngine::diarize`]), so the two models are
    /// never in memory together.
    ///
    /// # Errors
    ///
    /// When this build has no helper, Parakeet or the speech detector is not
    /// installed (with how to install it), or the helper refuses.
    pub async fn prepare(data_root: &Path) -> Result<Self, String> {
        if !crate::speechd::present(data_root) {
            return Err(format!(
                "Parakeet Unified is not installed, so the transcript was not cleaned up. {}",
                fotw_stt::speechd::install_hint(PARAKEET.id)
            ));
        }
        let detector = load_detector(data_root).await?;
        let client = crate::speechd::helper(data_root).await?;
        // Speaker labels are an addition: without the diarizer the cleaned
        // version is stored unlabelled and says so.
        let diarizer = crate::speechd::present_model(data_root, &DIARIZER);
        Self::start(client, PARAKEET.id, DIARIZER_ENGINE, detector, diarizer).await
    }

    /// [`prepare`](Self::prepare) with the helper, engines and detector
    /// given: load and warm `asr`, noting whether this engine loaded it.
    async fn start(
        client: SpeechdClient,
        asr: &'static str,
        diarizer_engine: &'static str,
        detector: Box<dyn SpeechDetector>,
        diarizer: bool,
    ) -> Result<Self, String> {
        let owns_asr = load(&client, asr)
            .await
            .map_err(|e| format!("Parakeet did not {e}"))?;
        Ok(Self {
            client,
            asr,
            diarizer_engine,
            detector: Arc::new(Mutex::new(detector)),
            diarizer,
            owns_asr: AtomicBool::new(owns_asr),
            owns_diarizer: AtomicBool::new(false),
            speaker_threshold: SPEAKER_THRESHOLD,
            bleed: Some(BleedParams::default()),
            request_timings: true,
            estimated: Default::default(),
        })
    }

    /// The speech helper and its models are macOS-only; elsewhere the
    /// cleanup is refused before anything starts.
    #[cfg(not(target_os = "macos"))]
    fn unsupported() -> String {
        "The transcript cleanup runs only on macOS.".into()
    }

    /// Diarize with this speaker threshold instead (evaluation).
    #[must_use]
    pub fn with_speaker_threshold(mut self, threshold: Option<f32>) -> Self {
        self.speaker_threshold = threshold;
        self
    }

    /// Suppress cross-track bleed with these settings instead, or not at
    /// all (evaluation).
    #[must_use]
    pub fn with_bleed(mut self, bleed: Option<BleedParams>) -> Self {
        self.bleed = bleed;
        self
    }

    /// Unload `engine` if this engine loaded it (`owned`). A refused unload
    /// (a stream still open) keeps it owned, so the job's final release
    /// tries again.
    async fn unload(&self, engine: &str, owned: &AtomicBool) {
        if !owned.load(Ordering::SeqCst) {
            return;
        }
        match self.client.unload(engine).await {
            Ok(()) => owned.store(false, Ordering::SeqCst),
            Err(e) => {
                crate::journal::record(&format!("cleanup  : {engine} stayed loaded: {e}"));
            }
        }
    }

    async fn send(stream: &SpeechStream, pcm: &[i16]) -> Result<(), String> {
        for frame in pcm.chunks(FRAME) {
            stream.send_audio(frame).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

impl PassEngine for HelperEngine {
    fn speech<'a>(&'a self, pcm: &'a [i16]) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>> {
        let detector = Arc::clone(&self.detector);
        let floats: Vec<f32> = pcm.iter().map(|&x| f32::from(x) / 32768.0).collect();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut detector = detector.lock().unwrap_or_else(|e| e.into_inner());
                let probabilities = detector.probabilities(&floats).map_err(|e| e.message)?;
                // Unpadded: the job pads.
                let config = GateConfig {
                    pad_ms: 0,
                    ..GateConfig::default()
                };
                Ok(speech_regions(&probabilities, floats.len(), &config)
                    .into_iter()
                    .map(|r| (r.start as u64, r.end as u64))
                    .collect())
            })
            .await
            .map_err(|e| e.to_string())?
        })
    }

    fn transcribe<'a>(&'a self, pcm: &'a [i16]) -> BoxFuture<'a, Result<Vec<TimedWord>, String>> {
        Box::pin(async move {
            let config = StreamConfig {
                options: StreamOptions::default().with_timings(self.request_timings),
                ..StreamConfig::new(self.asr, RATE)
            };
            let mut stream = self
                .client
                .open_stream(config)
                .await
                .map_err(|e| e.to_string())?;
            Self::send(&stream, pcm).await?;
            stream.finish().await.map_err(|e| e.to_string())?;
            let mut words = None;
            while let Some(event) = stream.next_event().await {
                match event {
                    StreamEvent::Final(result) => {
                        if result.segments.is_empty() && !result.text.trim().is_empty() {
                            // A helper without word timings: keep its words,
                            // with times spread over the window's speech.
                            self.estimated
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            words = Some(estimate_times(&result.text, pcm));
                            continue;
                        }
                        words = Some(
                            result
                                .segments
                                .into_iter()
                                .filter_map(|s| Some(TimedWord::new(s.text?, s.start_ms, s.end_ms)))
                                .collect(),
                        );
                    }
                    StreamEvent::Failed(f) => {
                        return Err(format!("Parakeet failed: {}", f.message));
                    }
                    StreamEvent::Closed(_) => break,
                    StreamEvent::Partial { .. } | StreamEvent::Vad { .. } => {}
                }
            }
            words.ok_or_else(|| "Parakeet closed the window without a result".to_owned())
        })
    }

    fn diarize(&self) -> BoxFuture<'_, Result<Option<Box<dyn Diarization>>, String>> {
        Box::pin(async move {
            if !self.diarizer {
                return Ok(None);
            }
            if load(&self.client, self.diarizer_engine)
                .await
                .map_err(|e| format!("the diarizer did not {e}"))?
            {
                self.owns_diarizer.store(true, Ordering::SeqCst);
            }
            let stream = self
                .client
                .open_stream(StreamConfig {
                    options: StreamOptions::default()
                        .with_speaker_threshold(self.speaker_threshold),
                    ..StreamConfig::new(self.diarizer_engine, RATE)
                })
                .await
                .map_err(|e| e.to_string())?;
            Ok(Some(Box::new(HelperDiarization {
                stream,
                sent: 0,
                acked_ms: 0,
            }) as Box<dyn Diarization>))
        })
    }

    fn provenance(&self) -> serde_json::Value {
        provenance()
    }

    fn release_recognizer(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.unload(self.asr, &self.owns_asr).await })
    }

    fn restore_recognizer(&self) -> BoxFuture<'_, ()> {
        // For the recording that started: loaded for it, so not this
        // engine's to unload any more.
        Box::pin(async move {
            self.owns_asr.store(false, Ordering::SeqCst);
            if let Err(e) = load(&self.client, self.asr).await {
                crate::journal::record(&format!("cleanup  : Parakeet did not {e}"));
            }
        })
    }

    fn release(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.unload(self.diarizer_engine, &self.owns_diarizer).await;
            self.unload(self.asr, &self.owns_asr).await;
        })
    }

    fn bleed(&self) -> Option<BleedParams> {
        self.bleed
    }

    fn notes(&self) -> Vec<String> {
        match self.estimated.load(std::sync::atomic::Ordering::SeqCst) {
            0 => Vec::new(),
            n => vec![format!(
                "the speech helper reported no word times for {n} window(s); their times were estimated"
            )],
        }
    }
}

/// Load and warm `engine` in the shared helper unless it is loaded already.
/// Returns whether this call loaded it. The error reads after "did not".
async fn load(client: &SpeechdClient, engine: &str) -> Result<bool, String> {
    let held = client
        .health()
        .await
        .map_err(|e| format!("load: {e}"))?
        .loaded
        .iter()
        .any(|e| e == engine);
    if held {
        return Ok(false);
    }
    client
        .load(engine)
        .await
        .map_err(|e| format!("load: {e}"))?;
    client
        .warmup(engine)
        .await
        .map_err(|e| format!("warm up: {e}"))?;
    Ok(true)
}

/// The pinned Silero detector live local transcription uses.
#[cfg(target_os = "macos")]
async fn load_detector(data_root: &Path) -> Result<Box<dyn SpeechDetector>, String> {
    let vad = crate::stt::verify_vad_model(data_root)?;
    tokio::task::spawn_blocking(move || {
        fotw_stt::whisper::SileroDetector::load(&vad)
            .map(|d| Box::new(d) as Box<dyn SpeechDetector>)
            .map_err(|e| e.message)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(not(target_os = "macos"))]
async fn load_detector(_data_root: &Path) -> Result<Box<dyn SpeechDetector>, String> {
    Err(HelperEngine::unsupported())
}

/// `text`'s words spread evenly over the audible part of `pcm` (the job has
/// silenced everything outside speech), for a helper without word timings.
fn estimate_times(text: &str, pcm: &[i16]) -> Vec<TimedWord> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let first = pcm.iter().position(|&x| x != 0).unwrap_or(0) as u64;
    let last = pcm
        .iter()
        .rposition(|&x| x != 0)
        .map_or(pcm.len(), |i| i + 1) as u64;
    let (from, to) = (
        first * 1_000 / u64::from(RATE),
        last * 1_000 / u64::from(RATE),
    );
    let n = words.len().max(1) as u64;
    let step = to.saturating_sub(from) / n;
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let start = from + step * i as u64;
            TimedWord::estimated(*w, start, start + step)
        })
        .collect()
}

/// One channel streamed to the helper's diarizer, paced by its per-window
/// acknowledgements so the helper never holds more than a minute of it.
struct HelperDiarization {
    stream: SpeechStream,
    sent: u64,
    acked_ms: u64,
}

impl Diarization for HelperDiarization {
    fn push<'a>(&'a mut self, pcm: &'a [i16]) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            for frame in pcm.chunks(FRAME) {
                self.stream
                    .send_audio(frame)
                    .await
                    .map_err(|e| e.to_string())?;
                self.sent += frame.len() as u64;
                let sent_ms = self.sent * 1_000 / u64::from(RATE);
                if sent_ms > self.acked_ms + DIARIZATION_MAX_UNACKED_MS {
                    self.acked_ms = self
                        .stream
                        .await_progress(sent_ms - DIARIZATION_MAX_UNACKED_MS)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        })
    }

    fn finish(mut self: Box<Self>) -> BoxFuture<'static, Result<Vec<SpeakerSpan>, String>> {
        Box::pin(async move {
            self.stream.finish().await.map_err(|e| e.to_string())?;
            let mut spans = None;
            while let Some(event) = self.stream.next_event().await {
                match event {
                    StreamEvent::Final(result) => {
                        spans = Some(
                            result
                                .segments
                                .into_iter()
                                .filter_map(|s| {
                                    Some(SpeakerSpan {
                                        start_ms: s.start_ms,
                                        end_ms: s.end_ms,
                                        speaker: s.speaker?,
                                    })
                                })
                                .collect(),
                        );
                    }
                    StreamEvent::Failed(f) => return Err(f.message),
                    StreamEvent::Closed(_) => break,
                    StreamEvent::Partial { .. } | StreamEvent::Vad { .. } => {}
                }
            }
            spans.ok_or_else(|| "the diarizer closed without a result".to_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speechd::testing::fake_launch;

    struct Silent;
    impl SpeechDetector for Silent {
        fn probabilities(&mut self, pcm: &[f32]) -> Result<Vec<f32>, fotw_stt::SttError> {
            Ok(vec![0.0; pcm.len().div_ceil(fotw_stt::vad::WINDOW_SAMPLES)])
        }
    }

    async fn client() -> SpeechdClient {
        let root = tempfile::tempdir().unwrap();
        crate::speechd::helper_with(&fake_launch(), root.path())
            .await
            .unwrap()
    }

    async fn start(client: SpeechdClient) -> HelperEngine {
        HelperEngine::start(
            client,
            "fake-words",
            "fake-diarizer",
            Box::new(Silent),
            true,
        )
        .await
        .unwrap()
        .with_speaker_threshold(None)
        .with_bleed(None)
    }

    async fn engine() -> HelperEngine {
        start(client().await).await
    }

    async fn loaded(client: &SpeechdClient) -> Vec<String> {
        let mut loaded = client.health().await.unwrap().loaded;
        loaded.sort();
        loaded
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parakeet_is_unloaded_before_the_diarizer_loads_and_nothing_stays_loaded() {
        let engine = engine().await;
        assert_eq!(loaded(&engine.client).await, ["fake-words"]);
        engine.release_recognizer().await;
        assert!(loaded(&engine.client).await.is_empty());
        let session = engine.diarize().await.unwrap().expect("a diarizer");
        assert_eq!(loaded(&engine.client).await, ["fake-diarizer"]);
        session.finish().await.unwrap();
        engine.release().await;
        assert!(
            loaded(&engine.client).await.is_empty(),
            "models left loaded"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_model_the_live_transcription_loaded_is_left_loaded() {
        // A recording loaded Parakeet in the shared helper before the
        // cleanup started; the cleanup does not take it away.
        let client = client().await;
        client.load("fake-words").await.unwrap();
        let engine = start(client).await;
        engine.release_recognizer().await;
        assert_eq!(loaded(&engine.client).await, ["fake-words"]);
        let session = engine.diarize().await.unwrap().expect("a diarizer");
        session.finish().await.unwrap();
        engine.release().await;
        assert_eq!(loaded(&engine.client).await, ["fake-words"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_released_recognizer_can_be_restored_for_a_recording() {
        let engine = engine().await;
        engine.release_recognizer().await;
        engine.restore_recognizer().await;
        assert_eq!(loaded(&engine.client).await, ["fake-words"]);
        // Restored for the recording, so the cleanup no longer owns it.
        engine.release().await;
        assert_eq!(loaded(&engine.client).await, ["fake-words"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn word_timings_come_back_from_the_helper_as_timed_words() {
        let engine = engine().await;
        // 30 s window: a word at 1.0-1.3 s and one straddling its end.
        let mut pcm = vec![0i16; 480_000];
        pcm[16_000..20_800].iter_mut().for_each(|x| *x = 1_200);
        pcm[475_200..].iter_mut().for_each(|x| *x = 900);
        let words = engine.transcribe(&pcm).await.unwrap();
        assert_eq!(
            words,
            [
                TimedWord::new("w1200", 1_000, 1_300),
                TimedWord::new("w900", 29_700, 30_000)
            ]
        );
        assert!(engine.speech(&pcm).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_helper_that_returns_text_without_word_times_still_yields_the_words() {
        let mut engine = engine().await;
        // Not asking for timings stands in for a helper that cannot give them.
        engine.request_timings = false;
        let mut pcm = vec![0i16; 480_000];
        pcm[16_000..20_800].iter_mut().for_each(|x| *x = 1_200);
        pcm[32_000..36_800].iter_mut().for_each(|x| *x = 700);
        let words = engine.transcribe(&pcm).await.unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["w1200", "w700"]);
        assert!(
            words.iter().all(|w| (1_000..=2_300).contains(&w.start_ms)),
            "{words:?}"
        );
        assert_eq!(engine.notes().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_long_channel_is_diarized_paced_by_the_helpers_acknowledgements() {
        let engine = engine().await;
        let mut session = engine.diarize().await.unwrap().expect("a diarizer");
        // Three minutes, more than the helper may hold unacknowledged:
        // a quiet speaker for the first half, a loud one for the second.
        let mut pcm = vec![2_000i16; 16_000 * 180];
        pcm[16_000 * 90..].iter_mut().for_each(|x| *x = 20_000);
        for piece in pcm.chunks(16_000 * 10) {
            session.push(piece).await.unwrap();
        }
        let spans = session.finish().await.unwrap();
        let shown: Vec<(u64, u64, &str)> = spans
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.speaker.as_str()))
            .collect();
        assert_eq!(shown, [(0, 90_000, "S1"), (90_000, 180_000, "S2")]);
    }
}

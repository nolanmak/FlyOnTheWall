//! The cleanup's engines in production: Silero VAD in-process (the same
//! pinned ggml detector that gates live local transcription, #242), and
//! Parakeet Unified with word timings plus the diarizer in the daemon's
//! shared speech helper (#243, docs/SPEECHD_PROTOCOL.md).

use std::path::Path;
use std::sync::{Arc, Mutex};

use fotw_speechd_client::message::StreamOptions;
use fotw_speechd_client::{
    DIARIZATION_MAX_UNACKED_MS, SpeechStream, SpeechdClient, StreamConfig, StreamEvent,
};
use fotw_stt::vad::{GateConfig, SpeechDetector, speech_regions};
use futures_util::future::BoxFuture;

use super::engine::{Diarization, PassEngine};
use super::merge::TimedWord;
use super::turns::SpeakerSpan;
use super::windows::RATE;
use crate::speechd::{DIARIZER, PARAKEET};

/// The diarizer's engine id in the helper.
pub const DIARIZER_ENGINE: &str = "diarizer";
/// The diarizer's speaker threshold for meetings: `None` is FluidAudio's
/// default.
pub const SPEAKER_THRESHOLD: Option<f32> = None;
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
    })
}

/// Parakeet and the diarizer in the helper, Silero beside them.
pub struct HelperEngine {
    client: SpeechdClient,
    asr: &'static str,
    diarizer_engine: &'static str,
    detector: Arc<Mutex<Box<dyn SpeechDetector>>>,
    diarizer: bool,
    speaker_threshold: Option<f32>,
}

impl HelperEngine {
    /// Load and warm Parakeet and, when it is installed, the diarizer, in
    /// the daemon's shared helper; load the speech detector.
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
        let vad = crate::stt::verify_vad_model(data_root)?;
        let detector = tokio::task::spawn_blocking(move || {
            fotw_stt::whisper::SileroDetector::load(&vad)
                .map(|d| Box::new(d) as Box<dyn SpeechDetector>)
                .map_err(|e| e.message)
        })
        .await
        .map_err(|e| e.to_string())??;
        let client = crate::speechd::helper(data_root).await?;
        client
            .load(PARAKEET.id)
            .await
            .map_err(|e| format!("Parakeet did not load: {e}"))?;
        client
            .warmup(PARAKEET.id)
            .await
            .map_err(|e| format!("Parakeet did not warm up: {e}"))?;
        // Speaker labels are an addition: without the diarizer the cleaned
        // version is stored unlabelled and says so.
        let diarizer = crate::speechd::present_model(data_root, &DIARIZER)
            && client.load(DIARIZER_ENGINE).await.is_ok()
            && client.warmup(DIARIZER_ENGINE).await.is_ok();
        Ok(Self {
            client,
            asr: PARAKEET.id,
            diarizer_engine: DIARIZER_ENGINE,
            detector: Arc::new(Mutex::new(detector)),
            diarizer,
            speaker_threshold: SPEAKER_THRESHOLD,
        })
    }

    /// Diarize with this speaker threshold instead (evaluation).
    #[must_use]
    pub fn with_speaker_threshold(mut self, threshold: Option<f32>) -> Self {
        self.speaker_threshold = threshold;
        self
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
                options: StreamOptions {
                    timings: true,
                    ..StreamOptions::default()
                },
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
                            return Err(
                                "the speech helper returned no word timings; update it".into()
                            );
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
            let stream = self
                .client
                .open_stream(StreamConfig {
                    options: StreamOptions {
                        speaker_threshold: self.speaker_threshold,
                        ..StreamOptions::default()
                    },
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

    async fn engine() -> HelperEngine {
        let root = tempfile::tempdir().unwrap();
        let client = crate::speechd::helper_with(&fake_launch(), root.path())
            .await
            .unwrap();
        for id in ["fake-words", "fake-diarizer"] {
            client.load(id).await.unwrap();
        }
        HelperEngine {
            client,
            asr: "fake-words",
            diarizer_engine: "fake-diarizer",
            detector: Arc::new(Mutex::new(Box::new(Silent))),
            diarizer: true,
            speaker_threshold: None,
        }
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

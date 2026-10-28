//! The speech gate between the endpointer and the model (#242), driven with
//! scripted speech probabilities instead of Silero.
//!
//! What the session relies on: a chunk the detector does not hear as speech
//! never reaches the model and never becomes a final, and a chunk that does is
//! timed exactly as it would be with no gate at all.

use std::sync::{Arc, Mutex};

use fotw_stt::vad::WINDOW_SAMPLES;
use fotw_stt::{
    GateConfig, GatedRecognizer, LocalStream, LocalStreamConfig, Recognized, Recognizer,
    RecognizerFactory, Source, SpeechDetector, StreamEvent, SttError, TranscriptSegment,
};

const RATE_PER_MS: usize = 16;

/// Remembers every chunk it is given and calls each one an utterance.
struct Scripted {
    heard: Arc<Mutex<Vec<Vec<f32>>>>,
}

impl Recognizer for Scripted {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.heard.lock().unwrap().push(pcm.to_vec());
        Ok(vec![Recognized {
            start_ms: 400,
            end_ms: 1_400,
            text: "Thank you.".into(),
        }])
    }
}

/// Scores each window by where it sits in the chunk: `speech` is the range of
/// chunk milliseconds it calls speech, at `high`; everything else is `low`.
struct Probabilities {
    speech: std::ops::Range<usize>,
    high: f32,
    low: f32,
}

impl SpeechDetector for Probabilities {
    fn probabilities(&mut self, pcm: &[f32]) -> Result<Vec<f32>, SttError> {
        Ok((0..pcm.len().div_ceil(WINDOW_SAMPLES))
            .map(|w| {
                let ms = w * WINDOW_SAMPLES / RATE_PER_MS;
                if self.speech.contains(&ms) {
                    self.high
                } else {
                    self.low
                }
            })
            .collect())
    }
}

struct Factory {
    heard: Arc<Mutex<Vec<Vec<f32>>>>,
    /// `None` is the ungated pipeline, for comparison.
    gate: Option<(std::ops::Range<usize>, f32, f32)>,
}

impl RecognizerFactory for Factory {
    fn model(&self) -> &str {
        "scripted"
    }

    fn language(&self) -> Option<&str> {
        None
    }

    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        let model = Box::new(Scripted {
            heard: Arc::clone(&self.heard),
        });
        Ok(match self.gate.clone() {
            None => model,
            Some((speech, high, low)) => Box::new(GatedRecognizer::new(
                Box::new(Probabilities { speech, high, low }),
                model,
                GateConfig::default(),
            )),
        })
    }
}

/// Loud enough, and varied enough, that the energy endpointer cuts it as an
/// utterance — the way music or a cafe is.
fn loud(ms: usize) -> Vec<i16> {
    (0..ms * RATE_PER_MS)
        .map(|i| {
            let amp = if (i / 1_920) % 2 == 0 { 8_000 } else { 800 };
            if (i / 20) % 2 == 0 { amp } else { -amp }
        })
        .collect()
}

fn silence(ms: usize) -> Vec<i16> {
    vec![0; ms * RATE_PER_MS]
}

/// 990 ms of silence, 3 s the endpointer calls speech, 990 ms of silence: one
/// chunk, starting at 690 ms with its preroll, 300 + 3000 + 720 ms long.
async fn run(
    gate: Option<(std::ops::Range<usize>, f32, f32)>,
) -> (Vec<TranscriptSegment>, Vec<Vec<f32>>) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(Factory {
        heard: Arc::clone(&heard),
        gate,
    });
    let config = LocalStreamConfig::new("s1", Source::System).with_session_offset_ms(5_000);
    let (stream, mut rx) = LocalStream::open(config, engine);
    let mut pcm = silence(990);
    pcm.extend(loud(3_000));
    pcm.extend(silence(990));
    for piece in pcm.chunks(320) {
        stream.write(piece);
    }
    stream.close().await.unwrap();
    let mut finals = Vec::new();
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Final(segment) = event {
            finals.push(segment);
        }
    }
    let heard = heard.lock().unwrap().clone();
    (finals, heard)
}

const CHUNK_MS: usize = 300 + 3_000 + 720;

#[tokio::test]
async fn probabilities_at_or_below_the_threshold_emit_nothing_and_never_reach_the_model() {
    for p in [0.0, 0.2, 0.49, 0.5] {
        let (finals, heard) = run(Some((0..CHUNK_MS, p, p))).await;
        assert!(finals.is_empty(), "p = {p}: {finals:?}");
        assert!(heard.is_empty(), "p = {p}: the model ran on non-speech");
    }
}

#[tokio::test]
async fn a_chunk_heard_as_speech_keeps_its_recording_time_offsets() {
    let (ungated, ungated_heard) = run(None).await;
    let (gated, gated_heard) = run(Some((0..CHUNK_MS, 0.9, 0.9))).await;
    assert_eq!(ungated.len(), 1);
    assert_eq!(gated.len(), 1);
    // Session offset 5000 + chunk start 690 + the model's 400..1400 ms.
    assert_eq!((gated[0].start_ms, gated[0].end_ms), (6_090, 7_090));
    assert_eq!(
        (gated[0].start_ms, gated[0].end_ms),
        (ungated[0].start_ms, ungated[0].end_ms)
    );
    assert_eq!(gated_heard, ungated_heard, "all-speech audio is untouched");
}

#[tokio::test]
async fn non_speech_around_a_speech_region_is_silenced_in_place() {
    // Speech only from 1500 ms to 2500 ms into the chunk.
    let (gated, heard) = run(Some((1_500..2_500, 0.9, 0.05))).await;
    assert_eq!(gated.len(), 1);
    assert_eq!((gated[0].start_ms, gated[0].end_ms), (6_090, 7_090));
    let audio = &heard[0];
    assert_eq!(
        audio.len(),
        CHUNK_MS * RATE_PER_MS,
        "the chunk keeps its length, so offsets inside it do not move"
    );
    let pad = GateConfig::default().pad_ms as usize;
    let first_kept = (1_500 - pad) * RATE_PER_MS;
    // Windows are 32 ms; allow one window of slack at each edge.
    let slack = WINDOW_SAMPLES;
    assert!(
        audio[..first_kept - slack].iter().all(|&s| s == 0.0),
        "audio before the padded region reached the model"
    );
    let last_kept = (2_500 + pad) * RATE_PER_MS;
    assert!(
        audio[last_kept + slack..].iter().all(|&s| s == 0.0),
        "audio after the padded region reached the model"
    );
    assert!(
        audio[first_kept + slack..last_kept - slack]
            .iter()
            .any(|&s| s != 0.0),
        "the speech itself was silenced"
    );
}

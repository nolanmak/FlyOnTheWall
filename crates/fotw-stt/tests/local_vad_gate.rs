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

// ---- Timing through the gate: the model's times must come from the audio.

/// Times its one segment by where the audio it was given is not silent, and
/// remembers the speech regions the gate reported.
struct AudioTimed {
    regions: Arc<Mutex<Vec<Vec<std::ops::Range<usize>>>>>,
}

impl Recognizer for AudioTimed {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        let first = pcm.iter().position(|&s| s != 0.0).unwrap_or(0);
        let last = pcm.iter().rposition(|&s| s != 0.0).map_or(0, |i| i + 1);
        Ok(vec![Recognized {
            start_ms: (first / RATE_PER_MS) as u64,
            end_ms: (last / RATE_PER_MS) as u64,
            text: "heard".into(),
        }])
    }

    fn transcribe_speech(
        &mut self,
        pcm: &[f32],
        speech: &[std::ops::Range<usize>],
    ) -> Result<Vec<Recognized>, SttError> {
        self.regions.lock().unwrap().push(speech.to_vec());
        self.transcribe(pcm)
    }
}

/// Scores the n-th chunk it sees with the n-th probability, every window.
struct PerChunk(std::collections::VecDeque<f32>);

impl SpeechDetector for PerChunk {
    fn probabilities(&mut self, pcm: &[f32]) -> Result<Vec<f32>, SttError> {
        let p = self.0.pop_front().unwrap_or(0.0);
        Ok(vec![p; pcm.len().div_ceil(WINDOW_SAMPLES)])
    }
}

struct Built<F: Fn() -> Box<dyn Recognizer> + Send + Sync>(F);

impl<F: Fn() -> Box<dyn Recognizer> + Send + Sync> RecognizerFactory for Built<F> {
    fn model(&self) -> &str {
        "scripted"
    }
    fn language(&self) -> Option<&str> {
        None
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok((self.0)())
    }
}

async fn finals_for(engine: Arc<dyn RecognizerFactory>, pcm: &[i16]) -> Vec<TranscriptSegment> {
    let config = LocalStreamConfig::new("s1", Source::System).with_session_offset_ms(5_000);
    let (stream, mut rx) = LocalStream::open(config, engine);
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
    finals
}

#[tokio::test]
async fn timestamps_follow_the_audio_through_a_silenced_chunk() {
    let regions = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&regions);
    let engine = Arc::new(Built(move || -> Box<dyn Recognizer> {
        Box::new(GatedRecognizer::new(
            Box::new(Probabilities {
                speech: 1_500..2_500,
                high: 0.9,
                low: 0.05,
            }),
            Box::new(AudioTimed {
                regions: Arc::clone(&seen),
            }),
            GateConfig::default(),
        ))
    }));
    let mut pcm = silence(990);
    pcm.extend(loud(3_000));
    pcm.extend(silence(990));
    let finals = finals_for(engine, &pcm).await;
    assert_eq!(finals.len(), 1);
    // Speech windows run from 1504 ms (window 47) to 2528 ms (window 79), and
    // 300 ms of padding either side leaves 1204..2828 ms of the chunk audible.
    // The chunk starts at 690 ms and the leg at 5000 ms.
    assert_eq!(
        (finals[0].start_ms, finals[0].end_ms),
        (5_000 + 690 + 1_204, 5_000 + 690 + 2_828),
        "the model's times must be where the kept audio is"
    );
    assert_eq!(
        *regions.lock().unwrap(),
        vec![vec![1_204 * RATE_PER_MS..2_828 * RATE_PER_MS]],
        "the model must be told which audio the gate heard as speech"
    );
}

#[tokio::test]
async fn a_dropped_chunk_does_not_shift_the_next_one() {
    let mut pcm = silence(990);
    pcm.extend(loud(1_500));
    pcm.extend(silence(990));
    pcm.extend(loud(1_500));
    pcm.extend(silence(990));
    let heard = Arc::new(Mutex::new(Vec::new()));
    let ungated = finals_for(
        Arc::new(Factory {
            heard: Arc::clone(&heard),
            gate: None,
        }),
        &pcm,
    )
    .await;
    assert_eq!(ungated.len(), 2);

    let gated_heard = Arc::new(Mutex::new(Vec::new()));
    let model_heard = Arc::clone(&gated_heard);
    let gated = finals_for(
        Arc::new(Built(move || -> Box<dyn Recognizer> {
            Box::new(GatedRecognizer::new(
                // First chunk is music, second is speech.
                Box::new(PerChunk([0.1, 0.9].into())),
                Box::new(Scripted {
                    heard: Arc::clone(&model_heard),
                }),
                GateConfig::default(),
            ))
        })),
        &pcm,
    )
    .await;
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated_heard.lock().unwrap().len(), 1);
    assert_eq!(
        (gated[0].start_ms, gated[0].end_ms),
        (ungated[1].start_ms, ungated[1].end_ms),
        "the surviving chunk keeps its own recording time"
    );
}

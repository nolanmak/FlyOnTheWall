//! The post-meeting pass's fakes (#246), identical to the Swift helper's
//! under `--fake-engines` (helpers/fotw-speechd/Sources/SpeechdCore/
//! PostPassFakes.swift) and specified in docs/SPEECHD_PROTOCOL.md:
//!
//! - `fake-diarizer`: a diarization stream that acknowledges every 10 s
//!   window and labels 100 ms blocks by loudness.
//! - `fake-words`: a batch recognizer whose words are runs of constant
//!   100 ms blocks, so windowed transcription is testable without a model.
//!   Like `parakeet-unified`, it sends the words' segments only with
//!   `options.timings`.
//!
//! Both answer at once on `finish`.

use fotw_speechd_client::message::{
    EngineInfo, EngineKind, FinalText, Reply, Segment, StreamOptions,
};

/// 100 ms at 16 kHz.
const BLOCK: usize = 1_600;
/// The diarizer's window: 10 s.
const WINDOW: u64 = 160_000;
/// `fake-diarizer`: 4 hours, like the real one.
const DIARIZER_MAX_SAMPLES: u64 = 4 * 3_600 * 16_000;
const DIARIZER_MAX_MS: u64 = 14_400_000;
/// `fake-words`: a batch stream's 10 minutes.
const WORDS_MAX_SAMPLES: u64 = 600 * 16_000;
/// Below this peak (int16) a block is silence; from it, a quiet voice.
const QUIET: u32 = 328;
/// From this peak a block is a loud voice.
const LOUD: u32 = 16_384;

pub const DIARIZER: &str = "fake-diarizer";
pub const WORDS: &str = "fake-words";

pub fn is_engine(id: &str) -> bool {
    id == DIARIZER || id == WORDS
}

pub fn engines() -> Vec<EngineInfo> {
    vec![
        EngineInfo {
            id: DIARIZER.to_owned(),
            kind: EngineKind::Diarization,
            partials: true,
            languages: Vec::new(),
            sample_rates: vec![16_000],
            model: None,
            max_audio_ms: Some(DIARIZER_MAX_MS),
        },
        EngineInfo {
            id: WORDS.to_owned(),
            kind: EngineKind::Batch,
            partials: false,
            languages: vec!["en".to_owned()],
            sample_rates: vec![16_000],
            model: None,
            max_audio_ms: Some(600_000),
        },
    ]
}

/// A run of equal 100 ms blocks: `[first, last]` block indices.
struct Run<K> {
    key: K,
    first: u64,
    last: u64,
}

/// Groups whole 100 ms blocks into maximal runs of one key; a block with no
/// key ends the current run. A trailing partial block is never classified.
pub struct Blocks<K> {
    pending: Vec<i16>,
    index: u64,
    runs: Vec<Run<K>>,
}

impl<K: PartialEq> Blocks<K> {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(BLOCK),
            index: 0,
            runs: Vec::new(),
        }
    }

    fn push(&mut self, pcm: &[i16], classify: impl Fn(&[i16]) -> Option<K>) {
        let mut rest = pcm;
        while !rest.is_empty() {
            let take = rest.len().min(BLOCK - self.pending.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() == BLOCK {
                if let Some(key) = classify(&self.pending) {
                    match self.runs.last_mut() {
                        Some(run) if run.key == key && run.last + 1 == self.index => {
                            run.last = self.index;
                        }
                        _ => self.runs.push(Run {
                            key,
                            first: self.index,
                            last: self.index,
                        }),
                    }
                }
                self.index += 1;
                self.pending.clear();
            }
        }
    }
}

/// `fake-diarizer`'s speaker classes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Voice {
    Quiet,
    Loud,
}

fn voice(block: &[i16]) -> Option<Voice> {
    let peak = block.iter().map(|s| u32::from(s.unsigned_abs())).max()?;
    if peak >= LOUD {
        Some(Voice::Loud)
    } else if peak >= QUIET {
        Some(Voice::Quiet)
    } else {
        None
    }
}

/// `fake-words`: the block's value, if every sample has the same nonzero one.
fn constant(block: &[i16]) -> Option<i16> {
    let first = *block.first()?;
    (first != 0 && block.iter().all(|&s| s == first)).then_some(first)
}

pub enum State {
    Diarizer {
        samples: u64,
        blocks: Blocks<Voice>,
    },
    Words {
        samples: u64,
        blocks: Blocks<i16>,
        /// `options.timings`: send the words' segments, as parakeet-unified
        /// does.
        timings: bool,
    },
}

/// What one audio frame did to a stream.
pub enum Outcome {
    /// Replies to send; the stream goes on.
    Ok(Vec<Reply>),
    /// The stream is past its bound: fail it `too_long` with this message.
    TooLong(&'static str),
}

impl State {
    pub fn new(engine: &str, options: &StreamOptions) -> Option<Self> {
        match engine {
            DIARIZER => Some(Self::Diarizer {
                samples: 0,
                blocks: Blocks::new(),
            }),
            WORDS => Some(Self::Words {
                samples: 0,
                blocks: Blocks::new(),
                timings: options.timings,
            }),
            _ => None,
        }
    }

    pub fn audio(&mut self, stream: u32, pcm: &[i16]) -> Outcome {
        match self {
            Self::Diarizer { samples, blocks } => {
                let before = *samples;
                let after = before + pcm.len() as u64;
                if after > DIARIZER_MAX_SAMPLES {
                    return Outcome::TooLong("a diarization stream holds at most 4 hours");
                }
                *samples = after;
                blocks.push(pcm, voice);
                // One acknowledgement per window this frame completed.
                let acks = (before / WINDOW + 1..=after / WINDOW)
                    .map(|window| Reply::Partial {
                        stream,
                        text: String::new(),
                        start_ms: 0,
                        end_ms: window * WINDOW / 16,
                        utterance: None,
                    })
                    .collect();
                Outcome::Ok(acks)
            }
            Self::Words {
                samples, blocks, ..
            } => {
                *samples += pcm.len() as u64;
                if *samples > WORDS_MAX_SAMPLES {
                    return Outcome::TooLong("a batch stream holds at most 10 minutes");
                }
                blocks.push(pcm, constant);
                Outcome::Ok(Vec::new())
            }
        }
    }

    pub fn finish(self, stream: u32, engine: &str) -> FinalText {
        let ms = |block: u64| block * 100;
        let (samples, text, segments) = match self {
            Self::Diarizer { samples, blocks } => {
                // The stream's speaker tracker: a label per voice, numbered in
                // order of first appearance over the whole stream, never per
                // window (the real engine's tracker lives for the stream).
                let mut seen: Vec<Voice> = Vec::new();
                let segments = blocks
                    .runs
                    .iter()
                    .map(|run| {
                        let n = seen.iter().position(|&v| v == run.key).unwrap_or_else(|| {
                            seen.push(run.key);
                            seen.len() - 1
                        });
                        Segment {
                            start_ms: ms(run.first),
                            end_ms: ms(run.last + 1),
                            text: None,
                            speaker: Some(format!("S{}", n + 1)),
                        }
                    })
                    .collect();
                (samples, String::new(), segments)
            }
            Self::Words {
                samples,
                blocks,
                timings,
            } => {
                let words: Vec<Segment> = blocks
                    .runs
                    .iter()
                    .map(|run| Segment {
                        start_ms: ms(run.first),
                        end_ms: ms(run.last + 1),
                        text: Some(format!("w{}", run.key)),
                        speaker: None,
                    })
                    .collect();
                let text = words
                    .iter()
                    .filter_map(|s| s.text.as_deref())
                    .collect::<Vec<_>>()
                    .join(" ");
                (samples, text, if timings { words } else { Vec::new() })
            }
        };
        FinalText {
            stream,
            text,
            start_ms: 0,
            end_ms: samples * 1000 / 16_000,
            segments,
            model: Some(format!("{engine}@fake")),
            utterance: None,
        }
    }
}

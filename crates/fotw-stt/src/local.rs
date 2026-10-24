//! On-device transcription (issue #33, STT-07): a capture leg's PCM in,
//! finals out, and nothing on the network.
//!
//! A local model is not a stream. whisper.cpp takes a buffer of audio and
//! returns its text, so this module does the two jobs a streaming provider
//! does for us on its own side:
//!
//! 1. **Endpointing.** The [`Endpointer`] cuts a leg's continuous PCM into
//!    utterance-sized chunks at the pauses between them, and drops the stretches
//!    in between. Silence is not just wasted compute here: Whisper fed silence
//!    or room tone invents text ("Thank you."), and the cheapest defence is to
//!    never show it any.
//! 2. **Pacing.** Inference is blocking CPU/GPU work, so each leg gets a worker
//!    thread of its own, off the async runtime. [`LocalStream::write`] never
//!    waits on it — audio queues while a chunk is being transcribed — because a
//!    recorder that stalls capture behind its transcriber has turned a late
//!    transcript into a lost recording.
//!
//! Timing follows the Deepgram transport's rule (#86): a leg's clock is how much
//! PCM it has been handed, and `session_offset_ms` puts it on the session clock.
//! A chunk remembers the sample it started at, so a segment's times are that
//! position plus what the model reports inside the chunk.
//!
//! The model itself sits behind [`Recognizer`], so everything here is tested
//! with a scripted recognizer and no weights; the whisper.cpp binding is in
//! [`crate::whisper`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;

use tokio::sync::{mpsc, oneshot};

use crate::speaker::ME;
use crate::stream::{StreamEvent, StreamState};
use crate::{Source, SttError, SttErrorClass, TranscriptSegment};

/// Provider id stamped on every local segment and transcript row.
pub const PROVIDER: &str = "whispercpp";

/// The one rate a leg is ever fed: the pump resamples both legs to it.
pub const SAMPLE_RATE: u32 = 16_000;

const SAMPLES_PER_MS: u64 = SAMPLE_RATE as u64 / 1_000;

/// How far behind real time a leg may fall before the user is told (60 s).
const BACKLOG_WARN_SAMPLES: u64 = 60 * SAMPLE_RATE as u64;

/// Where the [`Endpointer`] cuts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointConfig {
    /// Analysis frame. 30 ms is the usual VAD frame.
    pub frame_ms: u64,
    /// Audio kept from before speech starts, so a soft first syllable that sat
    /// under the threshold is not clipped.
    pub preroll_ms: u64,
    /// Silence that ends an utterance.
    pub hangover_ms: u64,
    /// Voiced audio an utterance needs to be worth transcribing. Shorter is a
    /// click, a cough or a keystroke.
    pub min_speech_ms: u64,
    /// Past this, an utterance ends at the next quiet frame.
    pub soft_max_ms: u64,
    /// Past this, an utterance ends regardless. Under Whisper's 30 s window.
    pub hard_max_ms: u64,
    /// How far above the tracked noise floor a frame must be to count as speech.
    pub margin_db: f32,
    /// The quietest level that can count as speech at all, in dBFS.
    pub speech_floor_db: f32,
    /// How long a sound has to hold steady before it is taken for the
    /// background. Longer than any pause-free stretch of one voice.
    pub floor_window_ms: u64,
    /// The level swing that separates a voice from the background. A fan, a
    /// hiss or a hum holds within a few dB; speech swings tens of dB a second.
    pub min_range_db: f32,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            frame_ms: 30,
            preroll_ms: 300,
            hangover_ms: 700,
            min_speech_ms: 250,
            soft_max_ms: 20_000,
            hard_max_ms: 28_000,
            margin_db: 10.0,
            speech_floor_db: -50.0,
            floor_window_ms: 2_000,
            min_range_db: 6.0,
        }
    }
}

/// One utterance's audio, ready for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The leg-clock sample this chunk starts at.
    pub start_sample: u64,
    /// 16 kHz mono PCM.
    pub samples: Vec<i16>,
}

impl Chunk {
    /// Where the chunk starts on the leg clock.
    #[must_use]
    pub fn start_ms(&self) -> u64 {
        self.start_sample / SAMPLES_PER_MS
    }

    /// How long the chunk is.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.samples.len() as u64 / SAMPLES_PER_MS
    }
}

struct Utterance {
    start_sample: u64,
    samples: Vec<i16>,
    voiced_ms: u64,
    silence_ms: u64,
    /// Level range from the onset on, preroll excluded.
    loudest_db: f32,
    quietest_db: f32,
}

/// An energy-based endpointer over 16 kHz mono PCM.
///
/// A frame is speech when it is [`EndpointConfig::margin_db`] above a noise
/// floor, and above an absolute floor so that a near-silent leg (the echo
/// gate's hush is literal zeros) does not promote its own hiss to speech.
///
/// The floor is learned two ways. Frames judged quiet pull it toward their
/// level. And a sound that has held within
/// [`min_range_db`](EndpointConfig::min_range_db) for a whole
/// [`floor_window_ms`](EndpointConfig::floor_window_ms) becomes the floor,
/// because speech swings and a fan does not. Without that, a background
/// louder than the absolute floor is taken for speech from its first frame,
/// and the floor, learning only from frames already judged quiet, never
/// learns it. Only a steady window moves it: raised during speech, the floor
/// would climb to the dips between syllables and cut words in two. An
/// utterance whose level hardly varies is dropped for the same reason, which
/// catches the couple of seconds before the floor catches up.
///
/// Deliberately simple: the model is the judge of what was said; this only
/// has to find where somebody was talking.
pub struct Endpointer {
    config: EndpointConfig,
    frame_len: usize,
    preroll_len: usize,
    /// A partial frame carried to the next `push`.
    pending: Vec<i16>,
    /// The leg-clock sample the next frame starts at.
    position: u64,
    noise_db: f32,
    /// Recent frame levels, newest last, to tell a steady background.
    recent: VecDeque<f32>,
    recent_len: usize,
    /// Recent audio while idle, for the preroll.
    preroll: VecDeque<i16>,
    utterance: Option<Utterance>,
}

impl Endpointer {
    /// An endpointer at the start of a leg.
    #[must_use]
    pub fn new(config: EndpointConfig) -> Self {
        let per_ms = SAMPLES_PER_MS as usize;
        Self {
            frame_len: config.frame_ms.max(1) as usize * per_ms,
            preroll_len: config.preroll_ms as usize * per_ms,
            config,
            pending: Vec::new(),
            position: 0,
            noise_db: -60.0,
            recent: VecDeque::new(),
            recent_len: (config.floor_window_ms / config.frame_ms.max(1)).max(1) as usize,
            preroll: VecDeque::new(),
            utterance: None,
        }
    }

    /// Feed audio, returning every utterance it completed.
    pub fn push(&mut self, pcm: &[i16]) -> Vec<Chunk> {
        let mut done = Vec::new();
        self.pending.extend_from_slice(pcm);
        let whole = self.pending.len() - self.pending.len() % self.frame_len;
        let frames: Vec<i16> = self.pending.drain(..whole).collect();
        for frame in frames.chunks(self.frame_len) {
            self.frame(frame, &mut done);
        }
        done
    }

    /// End the open utterance, if any, including a trailing partial frame.
    pub fn flush(&mut self) -> Option<Chunk> {
        let mut done = Vec::new();
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.frame(&rest, &mut done);
        }
        // Either that last frame ended the utterance or `finish` does; never both.
        done.extend(self.finish());
        done.pop()
    }

    fn frame(&mut self, frame: &[i16], done: &mut Vec<Chunk>) {
        let start = self.position;
        self.position += frame.len() as u64;
        let frame_ms = frame.len() as u64 / SAMPLES_PER_MS;
        let level = level_db(frame);
        self.recent.push_back(level);
        if self.recent.len() > self.recent_len {
            self.recent.pop_front();
        }
        // Only over a full window: the first frames of a leg may be its
        // first word, and a voice swings well past the range in any window.
        if self.recent.len() == self.recent_len {
            let (quietest, loudest) = self
                .recent
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &l| {
                    (lo.min(l), hi.max(l))
                });
            if loudest - quietest < self.config.min_range_db {
                self.noise_db = self.noise_db.max(quietest).clamp(-90.0, -25.0);
            }
        }
        let speech =
            level > (self.noise_db + self.config.margin_db).max(self.config.speech_floor_db);
        if !speech {
            // Falls fast, rises slowly: one loud breath must not raise the
            // floor enough to hide the next sentence.
            let rate = if level < self.noise_db { 0.3 } else { 0.02 };
            self.noise_db = (self.noise_db + rate * (level - self.noise_db)).clamp(-90.0, -25.0);
        }

        let Some(utterance) = self.utterance.as_mut() else {
            if speech {
                let preroll: Vec<i16> = self.preroll.drain(..).collect();
                let mut samples = Vec::with_capacity(preroll.len() + frame.len());
                samples.extend_from_slice(&preroll);
                samples.extend_from_slice(frame);
                self.utterance = Some(Utterance {
                    start_sample: start - preroll.len() as u64,
                    samples,
                    voiced_ms: frame_ms,
                    silence_ms: 0,
                    loudest_db: level,
                    quietest_db: level,
                });
            } else {
                self.preroll.extend(frame);
                let excess = self.preroll.len().saturating_sub(self.preroll_len);
                self.preroll.drain(..excess);
            }
            return;
        };

        utterance.samples.extend_from_slice(frame);
        utterance.loudest_db = utterance.loudest_db.max(level);
        utterance.quietest_db = utterance.quietest_db.min(level);
        if speech {
            utterance.voiced_ms += frame_ms;
            utterance.silence_ms = 0;
        } else {
            utterance.silence_ms += frame_ms;
        }
        let length_ms = utterance.samples.len() as u64 / SAMPLES_PER_MS;
        let ended = utterance.silence_ms >= self.config.hangover_ms
            || length_ms >= self.config.hard_max_ms
            || (length_ms >= self.config.soft_max_ms && !speech);
        if ended {
            done.extend(self.finish());
        }
    }

    fn finish(&mut self) -> Option<Chunk> {
        let utterance = self.utterance.take()?;
        let voiced = utterance.voiced_ms >= self.config.min_speech_ms;
        let varied = utterance.loudest_db - utterance.quietest_db >= self.config.min_range_db;
        (voiced && varied).then_some(Chunk {
            start_sample: utterance.start_sample,
            samples: utterance.samples,
        })
    }
}

/// A frame's RMS level in dBFS. Digital silence reads as -120.
fn level_db(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return -120.0;
    }
    let sum: f64 = frame
        .iter()
        .map(|&s| {
            let x = f64::from(s) / 32_768.0;
            x * x
        })
        .sum();
    let rms = (sum / frame.len() as f64).sqrt();
    (20.0 * rms.max(1e-6).log10()) as f32
}

/// Whether a model's output is an annotation rather than speech.
///
/// Whisper narrates what it hears when nobody is talking — `[BLANK_AUDIO]`,
/// `(upbeat music)`, `*laughs*`, a row of `♪`. Those are dropped; a line that
/// merely *contains* one (`so [inaudible] then`) is speech and is kept whole.
#[must_use]
pub fn is_non_speech(text: &str) -> bool {
    let mut depth = 0u32;
    let mut starred = false;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            '*' => starred = !starred,
            c if depth == 0 && !starred && c.is_alphanumeric() => return false,
            _ => {}
        }
    }
    true
}

/// One stretch of recognized speech, timed from the start of the audio the
/// model was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recognized {
    /// Offset into the chunk.
    pub start_ms: u64,
    /// Offset into the chunk.
    pub end_ms: u64,
    /// What was said.
    pub text: String,
}

/// A loaded model, ready to transcribe one leg's chunks.
pub trait Recognizer: Send {
    /// Transcribe 16 kHz mono audio in `[-1, 1]`.
    ///
    /// # Errors
    ///
    /// When the model fails on this chunk. The stream reports it and carries
    /// on with the next one.
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError>;
}

/// Makes a [`Recognizer`] per leg from one model held in memory once.
pub trait RecognizerFactory: Send + Sync {
    /// The model id recorded on each segment, e.g. `large-v3-turbo-q5_0`.
    fn model(&self) -> &str;

    /// The language the model was told to expect, `None` for detection.
    fn language(&self) -> Option<&str>;

    /// A recognizer for one leg. Called on that leg's worker thread, so a
    /// first call that loads the model from disk blocks no one else.
    ///
    /// # Errors
    ///
    /// When the model cannot be loaded. The leg then records without
    /// transcribing, and says so.
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError>;
}

/// What a [`LocalStream`] needs to transcribe one capture leg.
#[derive(Debug, Clone)]
pub struct LocalStreamConfig {
    /// The recording session this belongs to.
    pub session_id: String,
    /// Which capture leg this is. The mic leg is labelled `me`.
    pub source: Source,
    /// Where this leg's audio zero sits on the session clock (#86).
    pub session_offset_ms: u64,
    /// Where utterances are cut.
    pub endpoint: EndpointConfig,
}

impl LocalStreamConfig {
    /// A configuration for one capture leg.
    #[must_use]
    pub fn new(session_id: impl Into<String>, source: Source) -> Self {
        Self {
            session_id: session_id.into(),
            source,
            session_offset_ms: 0,
            endpoint: EndpointConfig::default(),
        }
    }

    /// Anchor this leg `session_offset_ms` into the session (#86).
    #[must_use]
    pub fn with_session_offset_ms(mut self, session_offset_ms: u64) -> Self {
        self.session_offset_ms = session_offset_ms;
        self
    }
}

/// Turn a chunk's recognized text into finals on the session clock.
///
/// Times are clamped into the chunk: Whisper pads its input to a 30-second
/// window and will happily time a segment past the audio it was given.
#[must_use]
pub fn segments_for(
    config: &LocalStreamConfig,
    model: &str,
    language: Option<&str>,
    chunk: &Chunk,
    recognized: Vec<Recognized>,
) -> Vec<TranscriptSegment> {
    let base = config.session_offset_ms + chunk.start_ms();
    let length = chunk.duration_ms();
    recognized
        .into_iter()
        .filter(|r| !is_non_speech(&r.text))
        .map(|r| {
            let mut segment =
                TranscriptSegment::new(config.session_id.clone(), config.source, PROVIDER, model);
            segment.speaker = (config.source == Source::Mic).then(|| ME.to_owned());
            segment.text = r.text.trim().to_owned();
            segment.start_ms = base + r.start_ms.min(length);
            segment.end_ms = (base + r.end_ms.min(length)).max(segment.start_ms);
            segment.language = language.map(str::to_owned);
            segment.is_final = true;
            segment
        })
        .collect()
}

enum Command {
    Audio(Vec<i16>),
    Flush(oneshot::Sender<()>),
    Close(oneshot::Sender<()>),
}

/// One leg transcribed on this machine.
///
/// The same shape as [`crate::DeepgramStream`] — `write`, `flush`, `close` and
/// an event channel — so the session drives either without knowing which.
/// Only finals are emitted: the model sees an utterance once, after it ends.
#[derive(Debug)]
pub struct LocalStream {
    commands: std_mpsc::Sender<Command>,
    closed: Arc<AtomicBool>,
    /// Samples written and not yet taken by the worker.
    backlog: Arc<AtomicU64>,
}

impl LocalStream {
    /// Start a leg's worker and return it with its event channel.
    ///
    /// Returns immediately; the model loads on the worker thread, and audio
    /// written meanwhile waits in the queue.
    #[must_use]
    pub fn open(
        config: LocalStreamConfig,
        engine: Arc<dyn RecognizerFactory>,
    ) -> (Self, mpsc::UnboundedReceiver<StreamEvent>) {
        let (command_tx, command_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let backlog = Arc::new(AtomicU64::new(0));
        let name = match config.source {
            Source::Mic => "fotw-stt-local-mic",
            Source::System => "fotw-stt-local-system",
        };
        let worker = Worker {
            endpointer: Endpointer::new(config.endpoint),
            config,
            engine,
            events: event_tx.clone(),
            backlog: Arc::clone(&backlog),
            warned_behind: false,
        };
        if let Err(error) = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || worker.run(&command_rx))
        {
            let _ = event_tx.send(StreamEvent::Error(
                SttError::new(
                    SttErrorClass::Server,
                    PROVIDER,
                    "could not start local transcription",
                )
                .with_detail(error.to_string()),
            ));
            let _ = event_tx.send(StreamEvent::State(StreamState::Closed));
        }
        (
            Self {
                commands: command_tx,
                closed: Arc::new(AtomicBool::new(false)),
                backlog,
            },
            event_rx,
        )
    }

    /// Hand the worker 16 kHz mono PCM. Never blocks and never fails.
    pub fn write(&self, pcm: &[i16]) {
        if pcm.is_empty() || self.closed.load(Ordering::SeqCst) {
            return;
        }
        self.backlog.fetch_add(pcm.len() as u64, Ordering::SeqCst);
        if self.commands.send(Command::Audio(pcm.to_vec())).is_err() {
            self.backlog.fetch_sub(pcm.len() as u64, Ordering::SeqCst);
        }
    }

    /// Transcribe the utterance in progress now rather than at its pause.
    ///
    /// Resolves once everything written before it has been transcribed.
    ///
    /// # Errors
    ///
    /// When the worker has already exited.
    pub async fn flush(&self) -> Result<(), SttError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.commands.send(Command::Flush(ack_tx)).is_err() {
            return Err(worker_gone("flush"));
        }
        ack_rx.await.map_err(|_| worker_gone("flush"))
    }

    /// Close the stream, once the audio already written has been transcribed.
    /// Idempotent.
    ///
    /// # Errors
    ///
    /// Never; the `Result` matches [`crate::DeepgramStream::close`].
    pub async fn close(&self) -> Result<(), SttError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.commands.send(Command::Close(ack_tx)).is_err() {
            return Ok(());
        }
        let _ = ack_rx.await;
        Ok(())
    }

    /// Whether [`close`](Self::close) has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

fn worker_gone(operation: &str) -> SttError {
    SttError::new(
        SttErrorClass::Server,
        PROVIDER,
        "local transcription is no longer running",
    )
    .with_detail(format!("{operation} was issued after the worker exited"))
}

struct Worker {
    config: LocalStreamConfig,
    engine: Arc<dyn RecognizerFactory>,
    endpointer: Endpointer,
    events: mpsc::UnboundedSender<StreamEvent>,
    backlog: Arc<AtomicU64>,
    warned_behind: bool,
}

impl Worker {
    fn run(mut self, commands: &std_mpsc::Receiver<Command>) {
        self.emit(StreamEvent::State(StreamState::Connecting));
        // A model that will not load leaves the leg recording untranscribed:
        // the audio is on disk either way, and the error says why.
        let mut recognizer = match self.engine.create() {
            Ok(recognizer) => {
                self.emit(StreamEvent::State(StreamState::Open));
                Some(recognizer)
            }
            Err(error) => {
                self.emit(StreamEvent::Error(error));
                None
            }
        };

        let mut pending_close = None;
        // Ends on `Close`, or when the handle is dropped without one.
        while let Ok(command) = commands.recv() {
            match command {
                Command::Audio(pcm) => {
                    self.backlog.fetch_sub(pcm.len() as u64, Ordering::SeqCst);
                    let Some(recognizer) = recognizer.as_deref_mut() else {
                        continue;
                    };
                    for chunk in self.endpointer.push(&pcm) {
                        self.transcribe(recognizer, &chunk);
                    }
                    self.check_backlog();
                }
                Command::Flush(ack) => {
                    if let Some(recognizer) = recognizer.as_deref_mut()
                        && let Some(chunk) = self.endpointer.flush()
                    {
                        self.transcribe(recognizer, &chunk);
                    }
                    let _ = ack.send(());
                }
                Command::Close(ack) => {
                    pending_close = Some(ack);
                    break;
                }
            }
        }

        // Whatever was mid-utterance when the meeting ended is still speech.
        if let Some(recognizer) = recognizer.as_deref_mut()
            && let Some(chunk) = self.endpointer.flush()
        {
            self.transcribe(recognizer, &chunk);
        }
        self.emit(StreamEvent::State(StreamState::Closed));
        if let Some(ack) = pending_close {
            let _ = ack.send(());
        }
    }

    fn transcribe(&self, recognizer: &mut dyn Recognizer, chunk: &Chunk) {
        let audio: Vec<f32> = chunk
            .samples
            .iter()
            .map(|&s| f32::from(s) / 32_768.0)
            .collect();
        match recognizer.transcribe(&audio) {
            Ok(recognized) => {
                for segment in segments_for(
                    &self.config,
                    self.engine.model(),
                    self.engine.language(),
                    chunk,
                    recognized,
                ) {
                    self.emit(StreamEvent::Final(segment));
                }
            }
            Err(error) => self.emit(StreamEvent::Error(error)),
        }
    }

    /// Say once when the model falls a minute behind the meeting, and again
    /// only after it has caught up.
    fn check_backlog(&mut self) {
        let behind = self.backlog.load(Ordering::SeqCst);
        if behind >= BACKLOG_WARN_SAMPLES && !self.warned_behind {
            self.warned_behind = true;
            self.emit(StreamEvent::Error(
                SttError::new(
                    SttErrorClass::Server,
                    PROVIDER,
                    "local transcription is running more than a minute behind the meeting; \
                     the rest of the transcript will arrive after you stop",
                )
                .with_detail(format!(
                    "{} s of audio queued",
                    behind / u64::from(SAMPLE_RATE)
                )),
            ));
        } else if behind < BACKLOG_WARN_SAMPLES / 2 {
            self.warned_behind = false;
        }
    }

    fn emit(&self, event: StreamEvent) {
        let _ = self.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUD: i16 = 8_000;

    /// Speech as the endpointer sees it: a square wave whose level swings 20 dB
    /// every 120 ms, the way syllables do. Only level matters here, and a
    /// steady tone is no longer speech (see `steady_noise_is_not_speech`).
    fn tone(ms: u64) -> Vec<i16> {
        (0..ms * SAMPLES_PER_MS)
            .map(|i| {
                let amp = if (i / 1_920) % 2 == 0 {
                    LOUD
                } else {
                    LOUD / 10
                };
                if (i / 20) % 2 == 0 { amp } else { -amp }
            })
            .collect()
    }

    fn quiet(ms: u64) -> Vec<i16> {
        vec![0; (ms * SAMPLES_PER_MS) as usize]
    }

    /// Steady broadband noise at about `db` dBFS: a fan, a hiss, a noisy room.
    fn noise(ms: u64, db: f32) -> Vec<i16> {
        // Uniform on [-a, a] has RMS a/sqrt(3).
        let a = 10f32.powf(db / 20.0) * 32_768.0 * 3f32.sqrt();
        let mut state = 0x2545_f491_u32;
        (0..ms * SAMPLES_PER_MS)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                ((state as f32 / u32::MAX as f32) * 2.0 - 1.0) * a
            })
            .map(|x| x as i16)
            .collect()
    }

    fn mixed(a: &[i16], b: &[i16]) -> Vec<i16> {
        a.iter().zip(b).map(|(x, y)| x.saturating_add(*y)).collect()
    }

    #[test]
    fn an_utterance_ends_at_the_pause_and_keeps_its_preroll() {
        // Frame-aligned (30 ms), so every boundary below is exact.
        let mut e = Endpointer::new(EndpointConfig::default());
        assert!(e.push(&quiet(990)).is_empty());
        assert!(e.push(&tone(1_500)).is_empty());
        let done = e.push(&quiet(990));
        assert_eq!(done.len(), 1);
        let chunk = &done[0];
        // Speech began at 990 ms; 300 ms of preroll comes with it.
        assert_eq!(chunk.start_ms(), 690);
        // Preroll + speech + the first 30 ms frame of silence past 700 ms.
        assert_eq!(chunk.duration_ms(), 300 + 1_500 + 720);
        assert!(e.flush().is_none());
    }

    #[test]
    fn a_click_is_not_an_utterance() {
        let mut e = Endpointer::new(EndpointConfig::default());
        let mut pcm = quiet(500);
        pcm.extend(tone(60));
        pcm.extend(quiet(1_000));
        assert!(e.push(&pcm).is_empty());
        assert!(e.flush().is_none());
    }

    #[test]
    fn continuous_speech_is_cut_under_the_model_window() {
        let mut e = Endpointer::new(EndpointConfig::default());
        let done = e.push(&tone(60_000));
        assert_eq!(done.len(), 2);
        for chunk in &done {
            assert_eq!(chunk.duration_ms(), 28_020);
        }
        // Contiguous: nothing between the cuts is lost.
        assert_eq!(done[1].start_sample, done[0].samples.len() as u64);
        let rest = e.flush().unwrap();
        assert_eq!(
            rest.start_sample,
            done[1].start_sample + done[1].samples.len() as u64
        );
        assert_eq!(rest.start_ms() + rest.duration_ms(), 60_000);
    }

    #[test]
    fn a_long_utterance_prefers_a_quiet_frame_past_the_soft_limit() {
        let mut e = Endpointer::new(EndpointConfig::default());
        let mut pcm = tone(21_000);
        pcm.extend(quiet(100));
        pcm.extend(tone(5_000));
        let done = e.push(&pcm);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].duration_ms(), 21_030);
    }

    #[test]
    fn the_dip_between_syllables_is_not_a_soft_cut() {
        // A voice never holds steady for the floor's window, so the floor
        // stays under its dips and a long utterance runs to its next real pause.
        let mut e = Endpointer::new(EndpointConfig::default());
        let done = e.push(&tone(27_000));
        assert!(
            done.is_empty(),
            "cut at {:?}",
            done.first().map(Chunk::duration_ms)
        );
    }

    #[test]
    fn steady_noise_is_not_speech() {
        // Louder than the absolute speech floor from the first frame: the case
        // where the floor used to never learn the room, and the model was fed
        // twenty seconds of fan.
        let mut e = Endpointer::new(EndpointConfig::default());
        let mut chunks = e.push(&noise(20_010, -45.0));
        chunks.extend(e.flush());
        assert!(chunks.is_empty(), "{} chunk(s)", chunks.len());
    }

    #[test]
    fn steady_noise_that_starts_mid_leg_is_not_speech() {
        let mut e = Endpointer::new(EndpointConfig::default());
        let mut chunks = e.push(&quiet(2_010));
        chunks.extend(e.push(&noise(10_020, -45.0)));
        chunks.extend(e.flush());
        assert!(chunks.is_empty(), "{} chunk(s)", chunks.len());
    }

    #[test]
    fn speech_over_steady_noise_is_still_found() {
        let mut e = Endpointer::new(EndpointConfig::default());
        let mut pcm = noise(3_000, -45.0);
        pcm.extend(mixed(&tone(1_500), &noise(1_500, -45.0)));
        pcm.extend(noise(3_000, -45.0));
        let mut chunks = e.push(&pcm);
        chunks.extend(e.flush());
        assert_eq!(chunks.len(), 1);
        // Speech began at 3000 ms, with 300 ms of preroll.
        assert_eq!(chunks[0].start_ms(), 2_700);
        assert_eq!(chunks[0].duration_ms(), 300 + 1_500 + 720);
    }

    #[test]
    fn odd_sized_writes_cut_in_the_same_places() {
        let mut pcm = quiet(800);
        pcm.extend(tone(2_000));
        pcm.extend(quiet(1_200));
        pcm.extend(tone(900));
        pcm.extend(quiet(1_000));

        let mut whole = Endpointer::new(EndpointConfig::default());
        let expected = whole.push(&pcm);

        let mut pieces = Endpointer::new(EndpointConfig::default());
        let got: Vec<Chunk> = pcm.chunks(333).flat_map(|p| pieces.push(p)).collect();
        assert_eq!(got, expected);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn flush_ends_the_open_utterance() {
        let mut e = Endpointer::new(EndpointConfig::default());
        assert!(e.push(&tone(1_005)).is_empty());
        let chunk = e.flush().unwrap();
        assert_eq!(chunk.start_sample, 0);
        assert_eq!(chunk.samples.len(), 1_005 * SAMPLES_PER_MS as usize);
    }

    #[test]
    fn annotations_are_not_speech() {
        for text in [
            "",
            "  ",
            "[BLANK_AUDIO]",
            "(upbeat music)",
            "[Music] (applause)",
            "*laughs*",
            "♪ ♪",
            "...",
        ] {
            assert!(is_non_speech(text), "{text:?}");
        }
        for text in ["Thanks.", "so [inaudible] then", "(laughs) yes", "42"] {
            assert!(!is_non_speech(text), "{text:?}");
        }
    }

    #[test]
    fn segments_land_on_the_session_clock_inside_their_chunk() {
        let config = LocalStreamConfig::new("s1", Source::Mic).with_session_offset_ms(250);
        let chunk = Chunk {
            start_sample: 2_000 * SAMPLES_PER_MS,
            samples: vec![0; (3_000 * SAMPLES_PER_MS) as usize],
        };
        let segments = segments_for(
            &config,
            "tiny",
            Some("en"),
            &chunk,
            vec![
                Recognized {
                    start_ms: 100,
                    end_ms: 1_200,
                    text: " Hello there.".into(),
                },
                Recognized {
                    start_ms: 1_300,
                    end_ms: 1_500,
                    text: "[BLANK_AUDIO]".into(),
                },
                Recognized {
                    start_ms: 1_500,
                    end_ms: 30_000,
                    text: "Past the end.".into(),
                },
            ],
        );
        assert_eq!(segments.len(), 2);
        let first = &segments[0];
        assert_eq!(first.text, "Hello there.");
        assert_eq!((first.start_ms, first.end_ms), (2_350, 3_450));
        assert_eq!(first.speaker.as_deref(), Some(ME));
        assert_eq!(first.provider, PROVIDER);
        assert_eq!(first.model, "tiny");
        assert_eq!(first.language.as_deref(), Some("en"));
        assert!(first.is_final);
        assert_eq!(segments[1].end_ms, 2_250 + 3_000);
        assert_ne!(first.id, segments[1].id);
    }

    #[test]
    fn the_system_leg_is_not_labelled_me() {
        let config = LocalStreamConfig::new("s1", Source::System);
        let chunk = Chunk {
            start_sample: 0,
            samples: vec![0; 16_000],
        };
        let segments = segments_for(
            &config,
            "tiny",
            None,
            &chunk,
            vec![Recognized {
                start_ms: 0,
                end_ms: 500,
                text: "Hi.".into(),
            }],
        );
        assert_eq!(segments[0].speaker, None);
        assert_eq!(segments[0].source, Source::System);
    }
}

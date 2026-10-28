//! `fotw-speechd-fake`: a model-free helper that speaks protocol version 1.
//!
//! The ordinary test suites spawn this in place of the Swift helper, so they
//! run with no model, no network and no Swift toolchain. It serves only the
//! fake engines from docs/SPEECHD_PROTOCOL.md, and the real helper serves the
//! same ones under `--fake-engines`.
//!
//! Like the real helper, it never blocks reading frames on an engine:
//! `fake-batch` takes over 2 s to finish, on a thread of its own, while audio,
//! health and other streams carry on.
//!
//! Three knobs exist only here, to test the client against a broken or newer
//! helper: `--announce-protocol <n>` answers hello with another version;
//! `--misbehave-on-health <oversized|not-json|missing-field|audio|fatal-error>`
//! answers a health request with that frame (`fatal-error` then exits 65); and
//! `--from-the-future` adds an engine of an unknown kind and precedes each
//! health answer with a message of an unknown type. `--slow-load-if <path>`
//! makes every `load` take 30 s (off the frame thread) while that file exists,
//! so a test can slow down only the loads a restart replays.
//! `--slow-health <ms>` answers every `health` that late, off the frame
//! thread, like a helper starved of CPU on a heavily loaded Mac.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, BufReader, BufWriter, Write};
use std::process::exit;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fotw_speechd_client::frame::{self, Frame, KIND_AUDIO, KIND_CONTROL, MAX_FRAME};
use fotw_speechd_client::message::{
    Capabilities, CloseReason, EngineInfo, EngineKind, ErrorCode, FORMAT_S16LE, FinalText,
    PROTOCOL_VERSION, ProgressPhase, Reply, Request, VadKind,
};

const MAX_STREAMS: usize = 8;
const SAMPLE_RATE: u32 = 16_000;
/// A batch stream's audio bound: 10 minutes.
const MAX_BATCH_SAMPLES: u64 = 600 * SAMPLE_RATE as u64;
const EXIT_PROTOCOL: i32 = 65;
const EXIT_CRASH: i32 = 70;
/// How long `fake-batch` takes to finish: longer than the client's stall and
/// heartbeat deadlines, so a helper that blocked on it would be killed.
const BATCH_FINISH: Duration = Duration::from_millis(2_500);

struct Args {
    announce: u32,
    misbehave: Option<String>,
    future: bool,
    slow_load_if: Option<std::path::PathBuf>,
    slow_health: Option<Duration>,
}

fn parse_args() -> Args {
    let mut args = Args {
        announce: PROTOCOL_VERSION,
        misbehave: None,
        future: false,
        slow_load_if: None,
        slow_health: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "serve" | "--fake-engines" => {}
            "--models-root" => {
                iter.next();
            }
            "--announce-protocol" => {
                args.announce = iter.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            "--misbehave-on-health" => args.misbehave = iter.next(),
            "--from-the-future" => args.future = true,
            "--slow-load-if" => args.slow_load_if = iter.next().map(Into::into),
            "--slow-health" => {
                args.slow_health = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .map(Duration::from_millis);
            }
            other => {
                eprintln!("fotw-speechd-fake: unknown argument {other}");
                exit(64);
            }
        }
    }
    args
}

/// Shared by the main loop and the threads that finish `fake-batch` streams.
#[derive(Clone)]
struct Out(Arc<Mutex<BufWriter<io::Stdout>>>);

impl Out {
    fn lock(&self) -> MutexGuard<'_, BufWriter<io::Stdout>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn raw(&self, bytes: &[u8]) {
        let mut out = self.lock();
        // The parent going away is not this process's problem to report.
        if out.write_all(bytes).and_then(|()| out.flush()).is_err() {
            exit(0);
        }
    }

    fn send(&self, reply: &Reply) {
        let bytes = frame::encode_control(reply).expect("replies are small");
        self.raw(&bytes);
    }

    fn fatal(&self, message: impl Into<String>) -> ! {
        self.send(&Reply::Error {
            id: None,
            stream: None,
            code: ErrorCode::Protocol,
            message: message.into(),
            fatal: true,
            supported: Vec::new(),
        });
        exit(EXIT_PROTOCOL);
    }

    fn fail(&self, id: u64, code: ErrorCode, message: &str) {
        self.send(&Reply::Error {
            id: Some(id),
            stream: None,
            code,
            message: message.to_owned(),
            fatal: false,
            supported: Vec::new(),
        });
    }

    fn progress(&self, id: u64, phase: ProgressPhase) {
        self.send(&Reply::Progress {
            id,
            phase,
            model: None,
            bytes: None,
            total: None,
        });
    }
}

/// fake-captions is pooled like parakeet-eou-320ms: at most two streams.
const CAPTION_POOL: usize = 2;

const ENGINES: [(&str, EngineKind, bool); 8] = [
    ("fake-batch", EngineKind::Batch, false),
    ("fake-stream", EngineKind::Streaming, true),
    ("fake-vad", EngineKind::Vad, false),
    ("fake-crash", EngineKind::Batch, false),
    ("fake-hang", EngineKind::Batch, false),
    ("fake-stall", EngineKind::Batch, false),
    ("fake-exit-after-final", EngineKind::Batch, false),
    ("fake-captions", EngineKind::Streaming, true),
];

fn capabilities(future: bool) -> serde_json::Value {
    let mut value = serde_json::to_value(Capabilities {
        max_frame_bytes: u32::try_from(MAX_FRAME).expect("1 MiB"),
        max_streams: u32::try_from(MAX_STREAMS).expect("small"),
        engines: ENGINES
            .iter()
            .map(|&(id, kind, partials)| EngineInfo {
                id: id.to_owned(),
                kind,
                partials,
                languages: vec!["en".to_owned()],
                sample_rates: vec![SAMPLE_RATE],
                model: None,
                max_audio_ms: (kind == EngineKind::Batch).then_some(600_000),
            })
            .collect(),
    })
    .expect("serializable");
    if future {
        value["engines"]
            .as_array_mut()
            .expect("engines")
            .push(serde_json::json!({
                "id": "fake-hologram", "kind": "hologram", "sample_rates": [SAMPLE_RATE],
                "telepathy": true,
            }));
    }
    value
}

struct Stream {
    engine: String,
    samples: u64,
    in_speech: bool,
    /// fake-captions: the utterance being spoken, where it began, and how
    /// much of it there is so far.
    utterance: u64,
    utterance_start: u64,
    utterance_samples: u64,
}

impl Stream {
    /// fake-captions: end the current utterance at sample `end`.
    fn end_utterance(&mut self, id: u32, end: u64) -> Option<Reply> {
        if self.utterance_samples == 0 {
            return None;
        }
        let reply = Reply::Final(FinalText {
            stream: id,
            text: format!(
                "utterance={} samples={}",
                self.utterance, self.utterance_samples
            ),
            start_ms: ms(self.utterance_start),
            end_ms: ms(end),
            segments: Vec::new(),
            model: Some("fake-captions@fake".to_owned()),
            utterance: Some(self.utterance),
        });
        self.utterance += 1;
        self.utterance_samples = 0;
        Some(reply)
    }
}

fn ms(samples: u64) -> u64 {
    samples * 1000 / u64::from(SAMPLE_RATE)
}

struct Helper {
    out: Out,
    started: Instant,
    misbehave: Option<String>,
    future: bool,
    slow_load_if: Option<std::path::PathBuf>,
    slow_health: Option<Duration>,
    /// `fake-batch` streams finishing on their own thread. Cancel removes one,
    /// and the thread only answers if it is still here, under this lock.
    finishing: Arc<Mutex<HashSet<u32>>>,
    loaded: BTreeSet<String>,
    streams: HashMap<u32, Stream>,
    /// Ids that were open once. Audio already in flight for one is dropped;
    /// audio for an id never opened is a protocol error.
    closed: HashSet<u32>,
}

impl Helper {
    fn request(&mut self, request: Request) {
        let out = self.out.clone();
        match request {
            Request::Hello { .. } => out.fatal("hello after the handshake"),
            Request::Load { id, engine } => {
                if ENGINES.iter().any(|e| e.0 == engine) {
                    out.progress(id, ProgressPhase::Load);
                    self.loaded.insert(engine.clone());
                    let loaded = Reply::Loaded {
                        id,
                        model: engine.clone(),
                        engine,
                        revision: "fake".to_owned(),
                        elapsed_ms: 0,
                    };
                    if self.slow_load_if.as_ref().is_some_and(|p| p.exists()) {
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_secs(30));
                            out.send(&loaded);
                        });
                    } else {
                        out.send(&loaded);
                    }
                } else {
                    out.fail(id, ErrorCode::UnknownEngine, "no such engine");
                }
            }
            Request::Warmup { id, engine } => {
                if self.loaded.contains(&engine) {
                    out.progress(id, ProgressPhase::Warmup);
                    out.send(&Reply::Warmed {
                        id,
                        engine,
                        elapsed_ms: 0,
                    });
                } else {
                    out.fail(id, ErrorCode::NotLoaded, "load it first");
                }
            }
            Request::Unload { id, engine } => {
                self.loaded.remove(&engine);
                out.send(&Reply::Unloaded { id, engine });
            }
            Request::OpenStream {
                id,
                stream,
                engine,
                sample_rate,
                format,
                channels,
                ..
            } => self.open(id, stream, engine, (sample_rate, &format, channels)),
            Request::Finish { stream } => self.finish(stream),
            Request::Cancel { stream } => {
                let was_finishing = lock(&self.finishing).remove(&stream);
                if self.streams.remove(&stream).is_some() || was_finishing {
                    self.closed.insert(stream);
                    out.send(&Reply::Closed {
                        stream,
                        reason: CloseReason::Cancelled,
                    });
                }
            }
            Request::Health { id } => {
                if let Some(how) = self.misbehave.as_deref() {
                    misbehave(how, &out);
                    return;
                }
                if self.future {
                    out.send_json(&serde_json::json!({"type": "telemetry", "cpu": 0.5}));
                }
                let finishing = lock(&self.finishing).len();
                let reply = Reply::Health {
                    id,
                    uptime_ms: u64::try_from(self.started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                    streams: u32::try_from(self.streams.len() + finishing).expect("bounded"),
                    loaded: self.loaded.iter().cloned().collect(),
                    rss_bytes: None,
                };
                match self.slow_health {
                    Some(delay) => {
                        std::thread::spawn(move || {
                            std::thread::sleep(delay);
                            out.send(&reply);
                        });
                    }
                    None => out.send(&reply),
                }
            }
            Request::Install { id, model, .. } => {
                out.fail(
                    id,
                    ErrorCode::InstallFailed,
                    &format!("the fake installs nothing ({model})"),
                );
            }
            Request::Shutdown { id } => {
                out.send(&Reply::Bye { id });
                exit(0);
            }
        }
    }

    fn open(&mut self, id: u64, stream: u32, engine: String, format: (u32, &str, u16)) {
        let out = &self.out;
        let refusal = if !ENGINES.iter().any(|e| e.0 == engine) {
            Some((ErrorCode::UnknownEngine, "no such engine"))
        } else if !self.loaded.contains(&engine) {
            Some((ErrorCode::NotLoaded, "load it first"))
        } else if format != (SAMPLE_RATE, FORMAT_S16LE, 1) {
            Some((ErrorCode::UnsupportedFormat, "16 kHz mono s16le only"))
        } else if self.streams.contains_key(&stream) || self.closed.contains(&stream) {
            Some((ErrorCode::StreamExists, "stream ids are single-use"))
        } else if engine == "fake-captions"
            && self
                .streams
                .values()
                .filter(|s| s.engine == "fake-captions")
                .count()
                >= CAPTION_POOL
        {
            Some((ErrorCode::Busy, "both fake-captions sessions are in use"))
        } else if self.streams.len() >= MAX_STREAMS {
            Some((ErrorCode::Busy, "too many streams"))
        } else {
            None
        };
        if let Some((code, message)) = refusal {
            out.fail(id, code, message);
            return;
        }
        self.streams.insert(
            stream,
            Stream {
                engine,
                samples: 0,
                in_speech: false,
                utterance: 1,
                utterance_start: 0,
                utterance_samples: 0,
            },
        );
        out.send(&Reply::StreamOpened { id, stream });
    }

    fn audio(&mut self, id: u32, pcm: &[i16]) {
        let out = self.out.clone();
        let Some(stream) = self.streams.get_mut(&id) else {
            if self.closed.contains(&id) {
                return;
            }
            out.fatal("audio for a stream that was never opened");
        };
        let start = stream.samples;
        stream.samples += pcm.len() as u64;
        match stream.engine.as_str() {
            "fake-crash" => exit(EXIT_CRASH),
            // Alive but deaf: stops reading and answering, like a wedged runtime.
            "fake-hang" => loop {
                std::thread::park();
            },
            // An utterance runs until an all-zero frame: a partial per frame
            // within it, a final at its end.
            "fake-captions" => {
                if pcm.iter().all(|&s| s == 0) {
                    if let Some(reply) = stream.end_utterance(id, start) {
                        out.send(&reply);
                    }
                } else {
                    if stream.utterance_samples == 0 {
                        stream.utterance_start = start;
                    }
                    stream.utterance_samples += pcm.len() as u64;
                    out.send(&Reply::Partial {
                        stream: id,
                        text: format!(
                            "utterance={} samples={}",
                            stream.utterance, stream.utterance_samples
                        ),
                        start_ms: ms(stream.utterance_start),
                        end_ms: ms(stream.samples),
                        utterance: Some(stream.utterance),
                    });
                }
            }
            "fake-stream" => out.send(&Reply::Partial {
                stream: id,
                text: format!("samples={}", stream.samples),
                start_ms: 0,
                end_ms: stream.samples * 1000 / u64::from(SAMPLE_RATE),
                utterance: Some(1),
            }),
            "fake-vad" => {
                let event = if stream.in_speech {
                    pcm.iter()
                        .all(|&s| s == 0)
                        .then_some((VadKind::SpeechEnd, start, 0.0))
                } else {
                    pcm.iter()
                        .position(|s| s.unsigned_abs() >= 16_384)
                        .map(|at| (VadKind::SpeechStart, start + at as u64, 1.0))
                };
                if let Some((event, sample, probability)) = event {
                    stream.in_speech = event == VadKind::SpeechStart;
                    out.send(&Reply::Vad {
                        stream: id,
                        event,
                        sample,
                        probability,
                    });
                }
            }
            _ if stream.samples > MAX_BATCH_SAMPLES => {
                self.streams.remove(&id);
                self.closed.insert(id);
                out.send(&Reply::Error {
                    id: None,
                    stream: Some(id),
                    code: ErrorCode::TooLong,
                    message: "a batch stream holds at most 10 minutes".to_owned(),
                    fatal: false,
                    supported: Vec::new(),
                });
            }
            _ => {}
        }
    }

    fn finish(&mut self, id: u32) {
        let out = self.out.clone();
        // A wedged engine: the stream never completes, the helper stays alive.
        if self
            .streams
            .get(&id)
            .is_some_and(|s| s.engine == "fake-stall")
        {
            return;
        }
        let Some(mut stream) = self.streams.remove(&id) else {
            return;
        };
        self.closed.insert(id);
        if stream.engine == "fake-captions" {
            let end = stream.samples;
            if let Some(reply) = stream.end_utterance(id, end) {
                out.send(&reply);
            }
            out.send(&Reply::Closed {
                stream: id,
                reason: CloseReason::Finished,
            });
            return;
        }
        let end_ms = stream.samples * 1000 / u64::from(SAMPLE_RATE);
        let final_text = FinalText {
            stream: id,
            text: format!("samples={}", stream.samples),
            start_ms: 0,
            end_ms,
            segments: Vec::new(),
            model: Some(format!("{}@fake", stream.engine)),
            utterance: (stream.engine == "fake-stream").then_some(1),
        };
        if stream.engine == "fake-batch" {
            // Slow, and off the frame-reading thread.
            lock(&self.finishing).insert(id);
            let finishing = self.finishing.clone();
            std::thread::spawn(move || {
                std::thread::sleep(BATCH_FINISH);
                let mut finishing = lock(&finishing);
                if finishing.remove(&id) {
                    out.send(&Reply::Final(final_text));
                    out.send(&Reply::Closed {
                        stream: id,
                        reason: CloseReason::Finished,
                    });
                }
            });
            return;
        }
        if stream.engine == "fake-vad" {
            if stream.in_speech {
                out.send(&Reply::Vad {
                    stream: id,
                    event: VadKind::SpeechEnd,
                    sample: stream.samples,
                    probability: 0.0,
                });
            }
        } else {
            out.send(&Reply::Final(final_text));
        }
        out.send(&Reply::Closed {
            stream: id,
            reason: CloseReason::Finished,
        });
        if stream.engine == "fake-exit-after-final" {
            exit(EXIT_CRASH);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Out {
    fn send_json(&self, value: &serde_json::Value) {
        self.raw(&frame::encode_control(value).expect("small"));
    }
}

fn misbehave(how: &str, out: &Out) {
    let bytes = match how {
        "oversized" => {
            let mut header = u32::try_from(MAX_FRAME + 1)
                .expect("fits")
                .to_be_bytes()
                .to_vec();
            header.push(KIND_CONTROL);
            header
        }
        "not-json" => [&[0, 0, 0, 3, KIND_CONTROL][..], b"{x"].concat(),
        // A known type without its required fields.
        "missing-field" => {
            frame::encode_control(&serde_json::json!({"type": "final"})).expect("small")
        }
        "fatal-error" => out.fatal("the fake was told to fail"),
        "audio" => vec![0, 0, 0, 7, KIND_AUDIO, 0, 0, 0, 1, 0, 0],
        other => {
            eprintln!("fotw-speechd-fake: unknown misbehaviour {other}");
            exit(64);
        }
    };
    out.raw(&bytes);
}

fn main() {
    let args = parse_args();
    let mut input = BufReader::new(io::stdin().lock());
    let out = Out(Arc::new(Mutex::new(BufWriter::new(io::stdout()))));

    match frame::read_frame(&mut input) {
        Ok(None) => exit(0),
        Ok(Some(Frame::Control(json))) => match serde_json::from_slice::<Request>(&json) {
            Ok(Request::Hello { id, protocol, .. }) if protocol != PROTOCOL_VERSION => {
                out.send(&Reply::Error {
                    id: Some(id),
                    stream: None,
                    code: ErrorCode::VersionMismatch,
                    message: format!("this helper speaks protocol {PROTOCOL_VERSION}"),
                    fatal: true,
                    supported: vec![PROTOCOL_VERSION],
                });
                exit(0);
            }
            Ok(Request::Hello { id, .. }) => out.send(&Reply::Hello {
                id,
                protocol: args.announce,
                helper: format!("fotw-speechd-fake/{}", env!("CARGO_PKG_VERSION")),
                capabilities: capabilities(args.future),
            }),
            _ => out.fatal("the first frame must be hello"),
        },
        Ok(Some(Frame::Audio { .. })) => out.fatal("the first frame must be hello"),
        Err(e) => out.fatal(e.to_string()),
    }

    let mut helper = Helper {
        out: out.clone(),
        started: Instant::now(),
        misbehave: args.misbehave,
        future: args.future,
        slow_load_if: args.slow_load_if,
        slow_health: args.slow_health,
        finishing: Arc::new(Mutex::new(HashSet::new())),
        loaded: BTreeSet::new(),
        streams: HashMap::new(),
        closed: HashSet::new(),
    };
    loop {
        match frame::read_frame(&mut input) {
            Ok(None) => exit(0),
            Ok(Some(Frame::Control(json))) => match serde_json::from_slice::<Request>(&json) {
                Ok(request) => helper.request(request),
                Err(e) => out.fatal(format!("malformed control message: {e}")),
            },
            Ok(Some(Frame::Audio { stream, pcm })) => helper.audio(stream, &pcm),
            Err(e) => out.fatal(e.to_string()),
        }
    }
}

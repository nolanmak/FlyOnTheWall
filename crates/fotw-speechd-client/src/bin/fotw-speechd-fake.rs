//! `fotw-speechd-fake`: a model-free helper that speaks protocol version 1.
//!
//! The ordinary test suites spawn this in place of the Swift helper, so they
//! run with no model, no network and no Swift toolchain. It serves only the
//! fake engines from docs/SPEECHD_PROTOCOL.md, and the real helper serves the
//! same ones under `--fake-engines`.
//!
//! Two knobs exist only here, to test the client against a broken helper:
//! `--announce-protocol <n>` answers hello with another version, and
//! `--misbehave-on-health <oversized|not-json|unknown-type|audio>` answers a
//! health request with that malformed frame.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, BufReader, BufWriter, Write};
use std::process::exit;
use std::time::Instant;

use fotw_speechd_client::frame::{self, Frame, KIND_AUDIO, KIND_CONTROL, MAX_FRAME};
use fotw_speechd_client::message::{
    Capabilities, CloseReason, EngineInfo, EngineKind, ErrorCode, FORMAT_S16LE, FinalText,
    PROTOCOL_VERSION, Reply, Request, VadKind,
};

const MAX_STREAMS: usize = 8;
const SAMPLE_RATE: u32 = 16_000;
/// A batch stream's audio bound: 10 minutes.
const MAX_BATCH_SAMPLES: u64 = 600 * SAMPLE_RATE as u64;
const EXIT_PROTOCOL: i32 = 65;
const EXIT_CRASH: i32 = 70;

struct Args {
    announce: u32,
    misbehave: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        announce: PROTOCOL_VERSION,
        misbehave: None,
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
            other => {
                eprintln!("fotw-speechd-fake: unknown argument {other}");
                exit(64);
            }
        }
    }
    args
}

struct Out(BufWriter<io::StdoutLock<'static>>);

impl Out {
    fn raw(&mut self, bytes: &[u8]) {
        // The parent going away is not this process's problem to report.
        if self
            .0
            .write_all(bytes)
            .and_then(|()| self.0.flush())
            .is_err()
        {
            exit(0);
        }
    }

    fn send(&mut self, reply: &Reply) {
        let bytes = frame::encode_control(reply).expect("replies are small");
        self.raw(&bytes);
    }

    fn fatal(&mut self, message: impl Into<String>) -> ! {
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

    fn fail(&mut self, id: u64, code: ErrorCode, message: &str) {
        self.send(&Reply::Error {
            id: Some(id),
            stream: None,
            code,
            message: message.to_owned(),
            fatal: false,
            supported: Vec::new(),
        });
    }
}

const ENGINES: [(&str, EngineKind, bool); 5] = [
    ("fake-batch", EngineKind::Batch, false),
    ("fake-stream", EngineKind::Streaming, true),
    ("fake-vad", EngineKind::Vad, false),
    ("fake-crash", EngineKind::Batch, false),
    ("fake-hang", EngineKind::Batch, false),
];

fn capabilities() -> Capabilities {
    Capabilities {
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
            })
            .collect(),
    }
}

struct Stream {
    engine: String,
    samples: u64,
    in_speech: bool,
}

struct Helper {
    started: Instant,
    misbehave: Option<String>,
    loaded: BTreeSet<String>,
    streams: HashMap<u32, Stream>,
    /// Ids that were open once. Audio already in flight for one is dropped;
    /// audio for an id never opened is a protocol error.
    closed: HashSet<u32>,
}

impl Helper {
    fn request(&mut self, request: Request, out: &mut Out) {
        match request {
            Request::Hello { .. } => out.fatal("hello after the handshake"),
            Request::Load { id, engine } => {
                if ENGINES.iter().any(|e| e.0 == engine) {
                    self.loaded.insert(engine.clone());
                    out.send(&Reply::Loaded {
                        id,
                        model: engine.clone(),
                        engine,
                        revision: "fake".to_owned(),
                        elapsed_ms: 0,
                    });
                } else {
                    out.fail(id, ErrorCode::UnknownEngine, "no such engine");
                }
            }
            Request::Warmup { id, engine } => {
                if self.loaded.contains(&engine) {
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
            } => self.open(id, stream, engine, (sample_rate, &format, channels), out),
            Request::Finish { stream } => self.finish(stream, out),
            Request::Cancel { stream } => {
                if self.streams.remove(&stream).is_some() {
                    self.closed.insert(stream);
                    out.send(&Reply::Closed {
                        stream,
                        reason: CloseReason::Cancelled,
                    });
                }
            }
            Request::Health { id } => {
                if let Some(how) = self.misbehave.as_deref() {
                    misbehave(how, out);
                    return;
                }
                out.send(&Reply::Health {
                    id,
                    uptime_ms: u64::try_from(self.started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                    streams: u32::try_from(self.streams.len()).expect("bounded"),
                    loaded: self.loaded.iter().cloned().collect(),
                    rss_bytes: None,
                });
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

    fn open(
        &mut self,
        id: u64,
        stream: u32,
        engine: String,
        format: (u32, &str, u16),
        out: &mut Out,
    ) {
        let refusal = if !ENGINES.iter().any(|e| e.0 == engine) {
            Some((ErrorCode::UnknownEngine, "no such engine"))
        } else if !self.loaded.contains(&engine) {
            Some((ErrorCode::NotLoaded, "load it first"))
        } else if format != (SAMPLE_RATE, FORMAT_S16LE, 1) {
            Some((ErrorCode::UnsupportedFormat, "16 kHz mono s16le only"))
        } else if self.streams.contains_key(&stream) || self.closed.contains(&stream) {
            Some((ErrorCode::StreamExists, "stream ids are single-use"))
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
            },
        );
        out.send(&Reply::StreamOpened { id, stream });
    }

    fn audio(&mut self, id: u32, pcm: &[i16], out: &mut Out) {
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
            "fake-stream" => out.send(&Reply::Partial {
                stream: id,
                text: format!("samples={}", stream.samples),
                start_ms: 0,
                end_ms: stream.samples * 1000 / u64::from(SAMPLE_RATE),
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

    fn finish(&mut self, id: u32, out: &mut Out) {
        let Some(stream) = self.streams.remove(&id) else {
            return;
        };
        self.closed.insert(id);
        let end_ms = stream.samples * 1000 / u64::from(SAMPLE_RATE);
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
            out.send(&Reply::Final(FinalText {
                stream: id,
                text: format!("samples={}", stream.samples),
                start_ms: 0,
                end_ms,
                segments: Vec::new(),
                model: Some(format!("{}@fake", stream.engine)),
            }));
        }
        out.send(&Reply::Closed {
            stream: id,
            reason: CloseReason::Finished,
        });
    }
}

fn misbehave(how: &str, out: &mut Out) {
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
        "unknown-type" => {
            frame::encode_control(&serde_json::json!({"type": "teleport"})).expect("small")
        }
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
    let mut out = Out(BufWriter::new(io::stdout().lock()));

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
                capabilities: serde_json::to_value(capabilities()).expect("serializable"),
            }),
            _ => out.fatal("the first frame must be hello"),
        },
        Ok(Some(Frame::Audio { .. })) => out.fatal("the first frame must be hello"),
        Err(e) => out.fatal(e.to_string()),
    }

    let mut helper = Helper {
        started: Instant::now(),
        misbehave: args.misbehave,
        loaded: BTreeSet::new(),
        streams: HashMap::new(),
        closed: HashSet::new(),
    };
    loop {
        match frame::read_frame(&mut input) {
            Ok(None) => exit(0),
            Ok(Some(Frame::Control(json))) => match serde_json::from_slice::<Request>(&json) {
                Ok(request) => helper.request(request, &mut out),
                Err(e) => out.fatal(format!("malformed control message: {e}")),
            },
            Ok(Some(Frame::Audio { stream, pcm })) => helper.audio(stream, &pcm, &mut out),
            Err(e) => out.fatal(e.to_string()),
        }
    }
}

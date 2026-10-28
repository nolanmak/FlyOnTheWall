//! Control messages, as JSON objects tagged by `type`.
//!
//! [`Request`] travels from the parent to the helper and [`Reply`] the other
//! way. Unknown `type`s and missing required fields fail to parse, which both
//! sides treat as a protocol error. Unknown *fields* are ignored, so a later
//! minor addition does not break an older peer.

use serde::{Deserialize, Serialize};

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The only PCM encoding version 1 carries.
pub const FORMAT_S16LE: &str = "s16le";

/// Parent → helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// First frame of every connection.
    Hello {
        /// Request id.
        id: u64,
        /// The parent's protocol version.
        protocol: u32,
        /// Free-form client name, for the helper's diagnostics.
        client: String,
    },
    /// Load an engine's model from local files.
    Load {
        /// Request id.
        id: u64,
        /// Engine id from the capabilities.
        engine: String,
    },
    /// Run one silent chunk through a loaded engine.
    Warmup {
        /// Request id.
        id: u64,
        /// Engine id.
        engine: String,
    },
    /// Release an engine's model.
    Unload {
        /// Request id.
        id: u64,
        /// Engine id.
        engine: String,
    },
    /// Open a stream on a loaded engine.
    OpenStream {
        /// Request id.
        id: u64,
        /// Client-chosen stream id, unique for the helper process.
        stream: u32,
        /// Engine id.
        engine: String,
        /// Samples per second of the PCM that follows.
        sample_rate: u32,
        /// Always [`FORMAT_S16LE`] in version 1.
        format: String,
        /// Always 1 in version 1.
        channels: u16,
        /// Engine options.
        #[serde(default)]
        options: StreamOptions,
    },
    /// No more audio for a stream.
    Finish {
        /// Stream id.
        stream: u32,
    },
    /// Drop a stream now.
    Cancel {
        /// Stream id.
        stream: u32,
    },
    /// Liveness and resource report.
    Health {
        /// Request id.
        id: u64,
    },
    /// Explicit model setup: download, or import from `from`.
    Install {
        /// Request id.
        id: u64,
        /// Model id.
        model: String,
        /// A local directory to import from instead of downloading.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// Unload everything and exit.
    Shutdown {
        /// Request id.
        id: u64,
    },
}

/// Per-stream engine options. Every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// VAD engines: also report every chunk's speech probability.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probabilities: bool,
    /// BCP 47 language hint, for multilingual engines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Helper → parent: responses (with `id`) and stream events (with `stream`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// Handshake answer. `capabilities` is parsed only once the version
    /// matches, so a different version's shape cannot fail the refusal.
    Hello {
        /// Request id.
        id: u64,
        /// The helper's protocol version.
        protocol: u32,
        /// Helper name and version.
        helper: String,
        /// See [`Capabilities`].
        capabilities: serde_json::Value,
    },
    /// An engine's model is loaded.
    Loaded {
        /// Request id.
        id: u64,
        /// Engine id.
        engine: String,
        /// Model id.
        model: String,
        /// Model revision.
        revision: String,
        /// Time the load took.
        elapsed_ms: u64,
    },
    /// Warmup ran.
    Warmed {
        /// Request id.
        id: u64,
        /// Engine id.
        engine: String,
        /// Time the warmup took.
        elapsed_ms: u64,
    },
    /// An engine was unloaded.
    Unloaded {
        /// Request id.
        id: u64,
        /// Engine id.
        engine: String,
    },
    /// A stream is open.
    StreamOpened {
        /// Request id.
        id: u64,
        /// Stream id.
        stream: u32,
    },
    /// Liveness report.
    Health {
        /// Request id.
        id: u64,
        /// Time since the helper started.
        uptime_ms: u64,
        /// Open streams.
        streams: u32,
        /// Loaded engine ids.
        loaded: Vec<String>,
        /// Resident memory, when the helper can read it.
        #[serde(default)]
        rss_bytes: Option<u64>,
    },
    /// Progress of a slow request: `install`, `load` or `warmup`.
    Progress {
        /// Request id.
        id: u64,
        /// What it is doing now.
        phase: ProgressPhase,
        /// Model id, for an install.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Bytes done, for a download or check.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        /// Bytes in total, for a download or check.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    /// A model is installed, validated and selected.
    Installed {
        /// Request id.
        id: u64,
        /// Model id.
        model: String,
        /// Installed revision.
        revision: String,
    },
    /// Shutdown acknowledged; the helper exits next.
    Bye {
        /// Request id.
        id: u64,
    },
    /// Provisional text.
    Partial {
        /// Stream id.
        stream: u32,
        /// Text so far.
        text: String,
        /// Start, in ms from the stream's first sample.
        #[serde(default)]
        start_ms: u64,
        /// End, in ms from the stream's first sample.
        #[serde(default)]
        end_ms: u64,
        /// The utterance this partial belongs to. A later partial or final
        /// with the same id replaces it in place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utterance: Option<u64>,
    },
    /// Committed text.
    Final(FinalText),
    /// A voice-activity event.
    Vad {
        /// Stream id.
        stream: u32,
        /// What happened.
        event: VadKind,
        /// Sample index from the stream's first sample.
        sample: u64,
        /// Speech probability of the chunk that decided it.
        probability: f32,
    },
    /// A failed request (`id`), a failed stream (`stream`), or a fatal
    /// connection error (neither).
    Error {
        /// Request id, when a request failed.
        #[serde(default)]
        id: Option<u64>,
        /// Stream id, when a stream failed.
        #[serde(default)]
        stream: Option<u32>,
        /// Machine-readable code.
        code: ErrorCode,
        /// Human-readable detail. Never transcript text.
        #[serde(default)]
        message: String,
        /// The helper exits after sending this.
        #[serde(default)]
        fatal: bool,
        /// With `version_mismatch`: the versions the helper speaks.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supported: Vec<u32>,
    },
    /// A stream ended; nothing more follows for it.
    Closed {
        /// Stream id.
        stream: u32,
        /// Why.
        reason: CloseReason,
    },
}

/// A `final` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalText {
    /// Stream id.
    pub stream: u32,
    /// The committed text.
    pub text: String,
    /// Start, in ms from the stream's first sample.
    #[serde(default)]
    pub start_ms: u64,
    /// End, in ms from the stream's first sample.
    #[serde(default)]
    pub end_ms: u64,
    /// Timed pieces, with speakers when the engine diarizes.
    #[serde(default)]
    pub segments: Vec<Segment>,
    /// `<model id>@<revision>`, for segment provenance.
    #[serde(default)]
    pub model: Option<String>,
    /// The utterance this commits, replacing its partials. Streaming engines
    /// number utterances from 1 within a stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utterance: Option<u64>,
}

/// What a slow request is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    /// Downloading model files.
    Download,
    /// Checking model files against their pins.
    Verify,
    /// Loading a model; the first load on a machine includes Core ML's
    /// compile and can take tens of seconds.
    Load,
    /// Running the warmup chunk.
    Warmup,
    /// A phase this version does not know.
    #[serde(other)]
    Other,
}

/// One timed piece of a final.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Start, in ms from the stream's first sample.
    pub start_ms: u64,
    /// End, in ms from the stream's first sample.
    pub end_ms: u64,
    /// Text, for recognizers.
    #[serde(default)]
    pub text: Option<String>,
    /// Speaker label, for diarizers.
    #[serde(default)]
    pub speaker: Option<String>,
}

/// Voice-activity event kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadKind {
    /// Speech began at `sample`.
    SpeechStart,
    /// Speech ended at `sample`.
    SpeechEnd,
    /// A per-chunk probability, when asked for.
    Probability,
}

/// Why a stream closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// After `finish`, with every result delivered.
    Finished,
    /// After `cancel`.
    Cancelled,
}

/// Error codes. Unknown codes from a newer helper parse as [`ErrorCode::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The peers speak different protocol versions.
    VersionMismatch,
    /// A malformed, oversized or unexpected frame.
    Protocol,
    /// A well-formed request that makes no sense.
    BadRequest,
    /// No such engine.
    UnknownEngine,
    /// The engine's model is not installed.
    ModelMissing,
    /// The installed model fails validation.
    ModelInvalid,
    /// The engine is not loaded.
    NotLoaded,
    /// The stream id is in use.
    StreamExists,
    /// No such stream.
    UnknownStream,
    /// Sample rate, format or channel count not supported.
    UnsupportedFormat,
    /// The stream's audio exceeds the engine's bound.
    TooLong,
    /// Too many streams.
    Busy,
    /// The engine failed.
    Engine,
    /// A model install failed; the previous selection stands.
    InstallFailed,
    /// Client side: the helper died.
    HelperCrashed,
    /// Client side: the helper stopped answering and was killed.
    HelperUnresponsive,
    /// Client side: the restart budget is spent.
    HelperFailed,
    /// Client side: a request missed its deadline.
    Timeout,
    /// A code this version does not know.
    #[serde(other)]
    Other,
}

/// What the helper serves, from its `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Largest frame either side may send, including the kind byte.
    pub max_frame_bytes: u32,
    /// Most streams open at once.
    pub max_streams: u32,
    /// Every engine.
    pub engines: Vec<EngineInfo>,
}

impl Capabilities {
    /// The engine with this id.
    #[must_use]
    pub fn engine(&self, id: &str) -> Option<&EngineInfo> {
        self.engines.iter().find(|engine| engine.id == id)
    }
}

/// One engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineInfo {
    /// Engine id.
    pub id: String,
    /// What it does.
    pub kind: EngineKind,
    /// Whether it sends `partial` events.
    #[serde(default)]
    pub partials: bool,
    /// Languages, as BCP 47 tags.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Accepted sample rates.
    pub sample_rates: Vec<u32>,
    /// The pinned model, for real engines.
    #[serde(default)]
    pub model: Option<ModelInfo>,
    /// Most audio one stream may carry, if the engine bounds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_audio_ms: Option<u64>,
}

/// Engine kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// Whole-utterance recognition on `finish`.
    Batch,
    /// Incremental recognition with partials.
    Streaming,
    /// Voice-activity detection.
    Vad,
    /// Speaker diarization.
    Diarization,
    /// A kind this version does not know. The client leaves such engines out
    /// of the capabilities it reports.
    #[serde(other)]
    Unknown,
}

/// A pinned model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model id.
    pub id: String,
    /// Hugging Face repository.
    pub repository: String,
    /// Full revision.
    pub revision: String,
    /// License, for the model UI and notices.
    pub license: String,
    /// Whether a validated copy is installed.
    pub installed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_with_their_wire_names() {
        let open = Request::OpenStream {
            id: 4,
            stream: 1,
            engine: "parakeet-unified".into(),
            sample_rate: 16_000,
            format: FORMAT_S16LE.into(),
            channels: 1,
            options: StreamOptions::default(),
        };
        let json = serde_json::to_value(&open).unwrap();
        assert_eq!(json["type"], "open_stream");
        assert_eq!(json["options"], serde_json::json!({}));
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), open);
    }

    #[test]
    fn an_unknown_type_or_missing_field_does_not_parse() {
        for bad in [
            r#"{"type":"teleport","id":1}"#,
            r#"{"type":"load","id":1}"#,
            r#"{"id":1}"#,
            r#"[1,2]"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
            assert!(serde_json::from_str::<Reply>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_unknown_error_code_is_other_not_a_parse_failure() {
        let reply: Reply =
            serde_json::from_str(r#"{"type":"error","id":3,"code":"quantum_flux","message":"x"}"#)
                .unwrap();
        assert!(matches!(
            reply,
            Reply::Error {
                id: Some(3),
                code: ErrorCode::Other,
                ..
            }
        ));
    }
}

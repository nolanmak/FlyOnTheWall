//! Parakeet Unified, and any other batch engine, through the `fotw-speechd`
//! helper (#243, #244).
//!
//! FluidAudio's models are Swift and Core ML, so they run in the helper and this
//! crate stays pure Rust. [`SpeechdEngine`] is a [`RecognizerFactory`] like the
//! whisper.cpp one: the leg's endpointer (and, in the daemon, the Silero speech
//! gate in front of it) still decides what an utterance is, and each utterance
//! becomes one batch stream on the helper — open, its PCM, `finish`, and the
//! `final` that answers it. Times come back from the stream's first sample, so
//! [`crate::local::segments_for`] puts them on the recording clock exactly as
//! it does Whisper's.
//!
//! The helper process belongs to the [`SpeechdClient`] passed in. A crash or a
//! hang fails the utterance in flight within the client's deadlines (1 s and
//! 3 s), the client restarts the helper and replays the load and warmup, and
//! the leg carries on with its next utterance; nothing here waits on a dead
//! helper.

use std::time::Duration;

use fotw_speechd_client::message::ErrorCode;
use fotw_speechd_client::{ClientError, SpeechdClient, StreamConfig, StreamEvent};

use crate::local::{Recognized, Recognizer, RecognizerFactory, SAMPLE_RATE};
use crate::{SttError, SttErrorClass};

/// Provider id stamped on every segment a helper engine transcribes.
pub const PROVIDER: &str = "fotw-speechd";

/// The default local meeting engine: Parakeet Unified 0.6B, English, int8.
pub const PARAKEET_UNIFIED: &str = "parakeet-unified";

/// The Hugging Face repository its Core ML weights come from.
pub const PARAKEET_UNIFIED_REPOSITORY: &str = "FluidInference/parakeet-unified-en-0.6b-coreml";

/// The pinned revision, which the helper's model store checks file by file.
pub const PARAKEET_UNIFIED_REVISION: &str = "d32e972dd4315f1dc3f6be28fb2aab0ab3e80358";

/// The weights' license, shown wherever the model is offered.
pub const PARAKEET_UNIFIED_LICENSE: &str = "CC-BY-4.0";

/// The language Parakeet Unified transcribes.
const LANGUAGE: &str = "en";

/// Ample for an utterance: the endpointer cuts at 28 s, and Parakeet runs at
/// a few percent of real time. The client's own crash and hang deadlines are
/// far shorter; this only bounds a helper that keeps answering heartbeats but
/// never finishes.
const UTTERANCE_TIMEOUT: Duration = Duration::from_secs(120);

/// What a user does about a model that is not installed.
#[must_use]
pub fn install_hint(engine: &str) -> String {
    format!(
        "Install it from Transcription settings, or run `fotwd stt models install {engine}` \
         while online; recording with it never downloads anything"
    )
}

/// A batch engine served by the helper, loaded and warmed.
///
/// Cheap to share between legs: each [`create`](RecognizerFactory::create)
/// is a handle on the same client, and the helper runs one inference at a
/// time per engine.
pub struct SpeechdEngine {
    client: SpeechdClient,
    engine: String,
    model: String,
    revision: String,
    runtime: tokio::runtime::Handle,
    utterance_timeout: Duration,
}

impl std::fmt::Debug for SpeechdEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeechdEngine")
            .field("engine", &self.engine)
            .field("model", &self.model)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl SpeechdEngine {
    /// Load `engine` in the helper and warm it, so the first utterance pays no
    /// Core ML compile. Call it while preparing a recording, before capture.
    ///
    /// Must run inside a Tokio runtime, which the recognizers then use from
    /// their leg's worker thread.
    ///
    /// # Errors
    ///
    /// [`SttErrorClass::Unsupported`], not retryable, with
    /// [`install_hint`] when the model is not installed or fails its check;
    /// [`SttErrorClass::Server`] when the helper does not start or answer.
    pub async fn prepare(client: SpeechdClient, engine: &str) -> Result<Self, SttError> {
        let caps = client
            .capabilities()
            .ok_or_else(|| server("the speech helper did not describe its engines", None))?;
        let info = caps.engine(engine).ok_or_else(|| {
            SttError::new(
                SttErrorClass::Unsupported,
                PROVIDER,
                format!("the speech helper does not serve {engine}"),
            )
            .not_retryable("unknown engine")
        })?;
        if info.model.as_ref().is_some_and(|model| !model.installed) {
            return Err(not_installed(engine, "not installed"));
        }
        let loaded = client.load(engine).await.map_err(|e| match e.code() {
            ErrorCode::ModelMissing => not_installed(engine, "not installed"),
            ErrorCode::ModelInvalid => not_installed(engine, "installed copy failed its check"),
            _ => server("the speech helper could not load the model", Some(&e)),
        })?;
        client
            .warmup(engine)
            .await
            .map_err(|e| server("the speech helper could not warm the model up", Some(&e)))?;
        Ok(Self {
            client,
            engine: engine.to_owned(),
            model: loaded.model,
            revision: loaded.revision,
            runtime: tokio::runtime::Handle::current(),
            utterance_timeout: UTTERANCE_TIMEOUT,
        })
    }

    /// The model revision the helper loaded, for provenance.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// The client that owns the helper.
    #[must_use]
    pub fn client(&self) -> &SpeechdClient {
        &self.client
    }

    /// How long one utterance may take before it is abandoned.
    #[must_use]
    pub fn with_utterance_timeout(mut self, timeout: Duration) -> Self {
        self.utterance_timeout = timeout;
        self
    }
}

impl RecognizerFactory for SpeechdEngine {
    fn model(&self) -> &str {
        &self.model
    }

    fn language(&self) -> Option<&str> {
        Some(LANGUAGE)
    }

    fn provider(&self) -> &str {
        PROVIDER
    }

    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(SpeechdRecognizer {
            client: self.client.clone(),
            engine: self.engine.clone(),
            provenance: format!("{}@{}", self.model, self.revision),
            runtime: self.runtime.clone(),
            timeout: self.utterance_timeout,
        }))
    }
}

/// One leg's handle: an utterance in, its final out.
struct SpeechdRecognizer {
    client: SpeechdClient,
    engine: String,
    provenance: String,
    runtime: tokio::runtime::Handle,
    timeout: Duration,
}

impl Recognizer for SpeechdRecognizer {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let pcm: Vec<i16> = pcm.iter().map(|&x| to_i16(x)).collect();
        // The leg's worker is an ordinary thread, so blocking it on the
        // runtime that owns the client is what it is for.
        self.runtime.block_on(async {
            tokio::time::timeout(self.timeout, self.utterance(&pcm))
                .await
                .unwrap_or_else(|_| {
                    Err(server(
                        "the speech helper did not finish an utterance in time",
                        None,
                    ))
                })
        })
    }
}

impl SpeechdRecognizer {
    async fn utterance(&self, pcm: &[i16]) -> Result<Vec<Recognized>, SttError> {
        let failed = |e: ClientError| server("the speech helper failed an utterance", Some(&e));
        let mut stream = self
            .client
            .open_stream(StreamConfig::new(&self.engine, SAMPLE_RATE))
            .await
            .map_err(failed)?;
        stream.send_audio(pcm).await.map_err(failed)?;
        stream.finish().await.map_err(failed)?;
        let mut recognized = Vec::new();
        loop {
            match stream.next_event().await {
                Some(StreamEvent::Final(final_text)) => {
                    // A restarted helper must not slip another model's text in
                    // under this one's name.
                    if let Some(model) = &final_text.model
                        && *model != self.provenance
                    {
                        return Err(server(
                            &format!(
                                "the speech helper answered with {model}, not {}",
                                self.provenance
                            ),
                            None,
                        ));
                    }
                    let text = final_text.text.trim();
                    if !text.is_empty() {
                        recognized.push(Recognized {
                            start_ms: final_text.start_ms,
                            end_ms: final_text.end_ms,
                            text: text.to_owned(),
                        });
                    }
                }
                Some(StreamEvent::Closed(_)) => return Ok(recognized),
                Some(StreamEvent::Failed(failure)) => {
                    return Err(SttError::new(
                        SttErrorClass::Server,
                        PROVIDER,
                        "the speech helper failed an utterance",
                    )
                    .with_detail(format!("{:?}: {}", failure.code, failure.message)));
                }
                Some(_) => {}
                None => {
                    return Err(server(
                        "the speech helper closed an utterance without a result",
                        None,
                    ));
                }
            }
        }
    }
}

/// `[-1, 1]` back to the s16le the helper takes. The worker made these floats
/// from i16 by dividing by 32768, so this is exact for them.
fn to_i16(x: f32) -> i16 {
    (x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

fn server(message: &str, cause: Option<&ClientError>) -> SttError {
    let error = SttError::new(SttErrorClass::Server, PROVIDER, message);
    match cause {
        Some(cause) => error.with_detail(cause.to_string()),
        None => error,
    }
}

fn not_installed(engine: &str, why: &str) -> SttError {
    SttError::new(
        SttErrorClass::Unsupported,
        PROVIDER,
        format!(
            "The {engine} speech model is {why}. {}.",
            install_hint(engine)
        ),
    )
    .not_retryable(why.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_survive_the_round_trip_through_floats() {
        for s in [i16::MIN, -12_345, -1, 0, 1, 12_345, i16::MAX] {
            assert_eq!(to_i16(f32::from(s) / 32_768.0), s);
        }
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(-2.0), i16::MIN);
    }

    #[test]
    fn the_hint_names_both_ways_to_install() {
        let hint = install_hint(PARAKEET_UNIFIED);
        assert!(hint.contains("Transcription settings"));
        assert!(hint.contains("`fotwd stt models install parakeet-unified`"));
    }
}

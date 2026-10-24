//! The whisper.cpp backend for [`crate::local`] (STT-07).
//!
//! macOS only for now, like the capture it serves: whisper.cpp is compiled from
//! source with Metal, and the other platforms' builds have no reason to pay for
//! a C++ toolchain until they have something to record.
//!
//! One model is loaded per session and shared by both legs. Each leg gets its
//! own `WhisperState` — the decoder's scratch memory — so the legs transcribe
//! independently without a second copy of the weights.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::local::{PROVIDER, Recognized, Recognizer, RecognizerFactory, SAMPLE_RATE};
use crate::{SttError, SttErrorClass};

/// whisper.cpp declines input under 100 ms and is unreliable on much under a
/// second, so shorter chunks are padded with silence to this.
const MIN_SAMPLES: usize = SAMPLE_RATE as usize;

/// whisper.cpp's own default: a window whose no-speech probability is above
/// this *and* whose decode was unconfident is treated as silence.
const NO_SPEECH_THRESHOLD: f32 = 0.6;

/// A ggml Whisper model on disk, loaded on first use.
pub struct WhisperEngine {
    path: PathBuf,
    model: String,
    language: Option<String>,
    threads: i32,
    context: OnceLock<Result<Arc<WhisperContext>, SttError>>,
}

impl std::fmt::Debug for WhisperEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperEngine")
            .field("path", &self.path)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl WhisperEngine {
    /// An engine for the model at `path`. Nothing is read until a leg starts.
    ///
    /// `language` is an ISO 639-1 code such as `en`; `None` detects it per
    /// utterance.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, language: Option<String>) -> Self {
        let path = path.into();
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get().min(8))
            .try_into()
            .unwrap_or(4);
        Self {
            model: model_name(&path),
            path,
            language,
            threads,
            context: OnceLock::new(),
        }
    }

    /// Load the model now rather than on the first leg.
    ///
    /// # Errors
    ///
    /// When the file is missing or is not a model whisper.cpp can read.
    pub fn load(&self) -> Result<(), SttError> {
        self.context().map(|_| ())
    }

    fn context(&self) -> Result<Arc<WhisperContext>, SttError> {
        self.context.get_or_init(|| load(&self.path)).clone()
    }
}

/// `ggml-large-v3-turbo-q5_0.bin` → `large-v3-turbo-q5_0`.
#[must_use]
pub fn model_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    stem.strip_prefix("ggml-").unwrap_or(&stem).to_owned()
}

fn load(path: &Path) -> Result<Arc<WhisperContext>, SttError> {
    // Without a logging backend this routes whisper.cpp's and ggml's chatter
    // nowhere, rather than into the daemon's stderr a line per layer.
    whisper_rs::install_logging_hooks();
    if !path.is_file() {
        return Err(SttError::new(
            SttErrorClass::Unsupported,
            PROVIDER,
            format!(
                "the local transcription model is missing ({}); run `fotwd stt` to fix",
                path.display()
            ),
        )
        .not_retryable("model file not found"));
    }
    let mut params = WhisperContextParameters::default();
    params.flash_attn(true);
    WhisperContext::new_with_params(path, params)
        .map(Arc::new)
        .map_err(|error| {
            SttError::new(
                SttErrorClass::Unsupported,
                PROVIDER,
                format!(
                    "the local transcription model could not be loaded ({})",
                    path.display()
                ),
            )
            .not_retryable(error.to_string())
        })
}

impl RecognizerFactory for WhisperEngine {
    fn model(&self) -> &str {
        &self.model
    }

    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        let state = self.context()?.create_state().map_err(|error| {
            SttError::new(
                SttErrorClass::Server,
                PROVIDER,
                "could not start the local transcription model",
            )
            .with_detail(error.to_string())
        })?;
        Ok(Box::new(WhisperRecognizer {
            state,
            language: self.language.clone(),
            threads: self.threads,
        }))
    }
}

struct WhisperRecognizer {
    state: WhisperState,
    language: Option<String>,
    threads: i32,
}

impl Recognizer for WhisperRecognizer {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        let mut padded = Vec::new();
        let audio = if pcm.len() < MIN_SAMPLES {
            padded.extend_from_slice(pcm);
            padded.resize(MIN_SAMPLES, 0.0);
            &padded
        } else {
            pcm
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_language(Some(self.language.as_deref().unwrap_or("auto")));
        params.set_translate(false);
        // Each chunk stands alone. Conditioning on the previous chunk's text
        // buys a little continuity and costs the repetition loops Whisper is
        // known for, which on a meeting transcript are worse.
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_no_speech_thold(NO_SPEECH_THRESHOLD);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        self.state.full(params, audio).map_err(|error| {
            SttError::new(
                SttErrorClass::Server,
                PROVIDER,
                "the local model failed on a stretch of audio",
            )
            .with_detail(error.to_string())
        })?;

        let mut recognized = Vec::new();
        for segment in self.state.as_iter() {
            let text = segment.to_str_lossy().map_err(|error| {
                SttError::new(
                    SttErrorClass::Server,
                    PROVIDER,
                    "the local model returned unreadable text",
                )
                .with_detail(error.to_string())
            })?;
            recognized.push(Recognized {
                start_ms: centiseconds_to_ms(segment.start_timestamp()),
                end_ms: centiseconds_to_ms(segment.end_timestamp()),
                text: text.trim().to_owned(),
            });
        }
        Ok(recognized)
    }
}

fn centiseconds_to_ms(cs: i64) -> u64 {
    u64::try_from(cs).unwrap_or(0) * 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_drop_the_ggml_prefix() {
        assert_eq!(
            model_name(Path::new("/m/ggml-large-v3-turbo-q5_0.bin")),
            "large-v3-turbo-q5_0"
        );
        assert_eq!(model_name(Path::new("custom.bin")), "custom");
    }

    #[test]
    fn a_missing_model_is_a_clear_error_not_a_crash() {
        let engine = WhisperEngine::new("/nonexistent/ggml-tiny.bin", None);
        let error = engine.create().err().expect("no model, no recognizer");
        assert_eq!(error.class, SttErrorClass::Unsupported);
        assert!(error.message.contains("/nonexistent/ggml-tiny.bin"));
        assert!(!error.retryable);
    }
}

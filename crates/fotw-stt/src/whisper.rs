//! The whisper.cpp backend for [`crate::local`] (STT-07).
//!
//! macOS only for now, like the capture it serves: whisper.cpp is compiled from
//! source with Metal, and the other platforms' builds have no reason to pay for
//! a C++ toolchain until they have something to record.
//!
//! One model is loaded per session and shared by both legs. Each leg gets its
//! own `WhisperState` — the decoder's scratch memory — so the legs transcribe
//! independently without a second copy of the weights.
//!
//! With [`WhisperEngine::with_speech_gate`], each leg also gets its own Silero
//! VAD context (whisper.cpp's ggml port, under a megabyte), and every chunk is
//! shown to it before Whisper (#242, see [`crate::vad`]). The daemon always
//! sets the gate; an ungated engine is for tools that compare against it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    WhisperVadContext, WhisperVadContextParams,
};

use crate::cleanup::is_filler_hallucination;
use crate::local::{PROVIDER, Recognized, Recognizer, RecognizerFactory, SAMPLE_RATE};
use crate::vad::{GateConfig, GatedRecognizer, SpeechDetector, WINDOW_SAMPLES};
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
    /// The Silero VAD model every chunk is gated by, if any.
    vad: Option<PathBuf>,
    gate: GateConfig,
}

impl std::fmt::Debug for WhisperEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperEngine")
            .field("path", &self.path)
            .field("language", &self.language)
            .field("vad", &self.vad)
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
            vad: None,
            gate: GateConfig::default(),
        }
    }

    /// Gate every chunk behind the Silero VAD model at `vad_model`: audio it
    /// does not hear as speech never reaches Whisper (#242).
    #[must_use]
    pub fn with_speech_gate(mut self, vad_model: impl Into<PathBuf>) -> Self {
        self.vad = Some(vad_model.into());
        self
    }

    /// Where the gate draws the line; [`GateConfig::default`] unless changed.
    #[must_use]
    pub fn with_gate_config(mut self, gate: GateConfig) -> Self {
        self.gate = gate;
        self
    }

    /// Load the model now rather than on the first leg, and check that the
    /// speech gate's model loads too.
    ///
    /// # Errors
    ///
    /// When either file is missing or is not a model whisper.cpp can read.
    pub fn load(&self) -> Result<(), SttError> {
        self.context()?;
        if let Some(vad) = &self.vad {
            SileroDetector::load(vad)?;
        }
        Ok(())
    }

    fn context(&self) -> Result<Arc<WhisperContext>, SttError> {
        validate_language(self.language.as_deref(), true)?;
        let context = self.context.get_or_init(|| load(&self.path)).clone()?;
        validate_language(self.language.as_deref(), context.is_multilingual())?;
        Ok(context)
    }
}

/// Normalize a requested language and check it against the loaded model.
/// `multilingual` must come from model metadata, never the file name.
pub fn validate_language(
    language: Option<&str>,
    multilingual: bool,
) -> Result<Option<String>, SttError> {
    let code = language.unwrap_or("auto").trim().to_ascii_lowercase();
    if code.is_empty() || code == "auto" {
        return Ok((!multilingual).then(|| "en".to_owned()));
    }
    let valid_code = (2..=3).contains(&code.len())
        && code.bytes().all(|b| b.is_ascii_lowercase())
        && whisper_rs::get_lang_id(&code).is_some();
    if !valid_code {
        return Err(SttError::new(SttErrorClass::Unsupported, PROVIDER,
            "language is not supported by the installed Whisper runtime; choose a supported code or auto")
            .not_retryable("unsupported language code"));
    }
    if !multilingual && code != "en" {
        return Err(SttError::new(SttErrorClass::Unsupported, PROVIDER,
            "this Whisper model supports English only; choose English or install a multilingual model")
            .not_retryable("language/model mismatch"));
    }
    Ok(Some(code))
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
        let context = self.context()?;
        let language = validate_language(self.language.as_deref(), context.is_multilingual())?;
        let state = context.create_state().map_err(|error| {
            SttError::new(
                SttErrorClass::Server,
                PROVIDER,
                "could not start the local transcription model",
            )
            .with_detail(error.to_string())
        })?;
        let recognizer = Box::new(WhisperRecognizer {
            state,
            language,
            threads: self.threads,
        });
        Ok(match &self.vad {
            None => recognizer,
            Some(vad) => Box::new(GatedRecognizer::new(
                Box::new(SileroDetector::load(vad)?),
                recognizer,
                self.gate,
            )),
        })
    }
}

/// Silero VAD through whisper.cpp, one context per leg.
pub struct SileroDetector {
    context: WhisperVadContext,
}

impl SileroDetector {
    /// Load the ggml Silero model at `path`.
    ///
    /// # Errors
    ///
    /// When the file is missing or is not a VAD model whisper.cpp can read.
    pub fn load(path: &Path) -> Result<Self, SttError> {
        whisper_rs::install_logging_hooks();
        let unusable = |why: &str| {
            SttError::new(
                SttErrorClass::Unsupported,
                PROVIDER,
                format!(
                    "the speech detector model {} ({}); run `fotwd stt models install silero-v5.1.2`",
                    why,
                    path.display()
                ),
            )
            .not_retryable(why.to_owned())
        };
        if !path.is_file() {
            return Err(unusable("is missing"));
        }
        let name = path
            .to_str()
            .ok_or_else(|| unusable("has a path that is not UTF-8"))?;
        let mut params = WhisperVadContextParams::default();
        // A 32 ms window at a time is too little work to spread or to ship
        // to the GPU, which Whisper itself is using.
        params.set_n_threads(1);
        params.set_use_gpu(false);
        WhisperVadContext::new(name, params)
            .map(|context| Self { context })
            .map_err(|_| unusable("could not be loaded"))
    }
}

impl SpeechDetector for SileroDetector {
    fn probabilities(&mut self, pcm: &[f32]) -> Result<Vec<f32>, SttError> {
        let failed = || {
            SttError::new(
                SttErrorClass::Server,
                PROVIDER,
                "the speech detector failed on a stretch of audio",
            )
        };
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        self.context.detect_speech(pcm).map_err(|_| failed())?;
        let probabilities = self.context.probabilities();
        if probabilities.len() != pcm.len().div_ceil(WINDOW_SAMPLES) {
            return Err(failed().with_detail(format!(
                "{} probabilities for {} samples; expected one per {WINDOW_SAMPLES}",
                probabilities.len(),
                pcm.len()
            )));
        }
        Ok(probabilities.to_vec())
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
            // Whisper's stock phrases over audio it doubted was speech.
            if is_filler_hallucination(&text, segment.no_speech_probability()) {
                continue;
            }
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
    fn languages_are_validated_against_runtime_and_model_capabilities() {
        assert_eq!(
            validate_language(Some(" EN "), true).unwrap().as_deref(),
            Some("en")
        );
        assert_eq!(validate_language(Some("AUTO"), true).unwrap(), None);
        assert_eq!(
            validate_language(None, false).unwrap().as_deref(),
            Some("en")
        );
        assert_eq!(
            validate_language(Some("auto"), false).unwrap().as_deref(),
            Some("en")
        );
        assert_eq!(
            validate_language(Some("de"), true).unwrap().as_deref(),
            Some("de")
        );
        for language in ["zz", "english", "en\0", "de"] {
            let error = validate_language(Some(language), false).unwrap_err();
            assert_eq!(error.class, SttErrorClass::Unsupported);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn unsupported_language_is_rejected_before_opening_a_model() {
        let error = WhisperEngine::new("/nonexistent/model.bin", Some("zz".into()))
            .load()
            .unwrap_err();
        assert_eq!(error.class, SttErrorClass::Unsupported);
        assert!(error.message.contains("language"));
        assert!(!error.retryable);
    }

    #[test]
    #[ignore = "requires an explicitly supplied English-only GGML model; no downloads"]
    fn real_english_model_rejects_non_english_from_metadata() {
        let path = std::env::var_os("FOTW_WHISPER_ENGLISH_MODEL")
            .expect("set FOTW_WHISPER_ENGLISH_MODEL to an English-only model");
        let engine = WhisperEngine::new(PathBuf::from(path), Some("de".into()));
        let error = engine.load().unwrap_err();
        assert_eq!(error.class, SttErrorClass::Unsupported);
        assert!(error.message.contains("English only"));
        let context = engine.context.get().unwrap().as_ref().unwrap();
        assert!(!context.is_multilingual());
        assert_eq!(
            validate_language(None, context.is_multilingual())
                .unwrap()
                .as_deref(),
            Some("en")
        );
    }

    #[test]
    fn a_missing_speech_detector_is_a_clear_error_not_a_crash() {
        let error = SileroDetector::load(Path::new("/nonexistent/ggml-silero-v5.1.2.bin"))
            .err()
            .expect("no model, no detector");
        assert_eq!(error.class, SttErrorClass::Unsupported);
        assert!(error.message.contains("silero-v5.1.2"), "{}", error.message);
        assert!(!error.retryable);
    }

    #[test]
    #[ignore = "requires FOTW_SILERO_VAD_MODEL pointing at ggml-silero-v5.1.2.bin; no downloads"]
    fn real_silero_scores_digital_silence_as_non_speech() {
        let path = std::env::var_os("FOTW_SILERO_VAD_MODEL")
            .expect("set FOTW_SILERO_VAD_MODEL to the ggml Silero model");
        let mut detector = SileroDetector::load(Path::new(&path)).unwrap();
        let probabilities = detector.probabilities(&vec![0.0; 16_000]).unwrap();
        assert_eq!(probabilities.len(), 16_000usize.div_ceil(WINDOW_SAMPLES));
        assert!(probabilities.iter().all(|&p| p < 0.5), "{probabilities:?}");
    }

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

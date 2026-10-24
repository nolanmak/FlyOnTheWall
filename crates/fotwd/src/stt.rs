//! Which speech-to-text engine a recording uses (#33).
//!
//! Deepgram stays the default: with a key stored it is what the app has always
//! done, and without one a meeting records untranscribed. `fotwd stt local`
//! switches to whisper.cpp on this machine instead, and that choice is the
//! user's word that meeting audio must not leave it. So it fails closed: a
//! local engine whose model is missing records without a transcript and says
//! why, and nothing here ever falls back to Deepgram on its own.
//!
//! No model ships with the app and the daemon never downloads one — the
//! egress allowlist (`transport.rs`) names providers the user holds a key for,
//! and a model host is not one. The user fetches the file; [`verify_model`]
//! checks it against the checksum published beside it.

use std::io::Read;
use std::path::{Path, PathBuf};

use fotw_store::Db;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session::Transcription;

/// The settings row that holds [`SttSettings`].
pub const SETTINGS_KEY: &str = "stt";

/// The model `fotwd stt local` uses when not told otherwise.
///
/// Near large-v3 accuracy at a third of its size, and several times faster
/// than real time with Metal on any Apple-silicon Mac.
pub const DEFAULT_MODEL: &str = "large-v3-turbo-q5_0";

/// Which engine transcribes a recording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SttEngine {
    /// Deepgram, when a key is stored.
    #[default]
    Deepgram,
    /// whisper.cpp on this machine.
    Local,
}

/// The stored engine choice.
///
/// Missing or unparseable reads as default — a fresh library, never an error —
/// the same shape as the summary engine's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttSettings {
    /// Which engine.
    pub engine: SttEngine,
    /// The ggml model file the local engine loads, as an absolute path.
    pub model: String,
    /// ISO 639-1 code for the local engine. Empty detects per utterance.
    pub language: String,
}

impl SttSettings {
    /// The settings in force for this library.
    #[must_use]
    pub fn read(db: &Db) -> Self {
        db.get_setting(SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    /// Store these settings.
    ///
    /// # Errors
    ///
    /// When the library cannot be written.
    pub fn write(&self, db: &mut Db) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        db.put_setting(SETTINGS_KEY, &json)
            .map_err(|e| e.to_string())
    }

    /// The model the local engine loads.
    #[must_use]
    pub fn model_path(&self, data_root: &Path) -> PathBuf {
        if self.model.is_empty() {
            models_dir(data_root).join(model_file_name(DEFAULT_MODEL))
        } else {
            PathBuf::from(&self.model)
        }
    }

    /// The language the local engine is told to expect, `None` to detect it.
    #[must_use]
    pub fn language(&self) -> Option<String> {
        let code = self.language.trim();
        (!code.is_empty() && code != "auto").then(|| code.to_ascii_lowercase())
    }
}

/// A model whose published file we can check byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownModel {
    /// The name after `ggml-`, e.g. `large-v3-turbo-q5_0`.
    pub name: &'static str,
    /// The file's size.
    pub bytes: u64,
    /// SHA-256 of the file, lowercase hex, as Hugging Face publishes it.
    pub sha256: &'static str,
    /// One line on when to pick it.
    pub note: &'static str,
}

/// The models `fotwd stt local` knows by name, from
/// `huggingface.co/ggerganov/whisper.cpp`.
pub const KNOWN_MODELS: &[KnownModel] = &[
    KnownModel {
        name: "large-v3-turbo-q5_0",
        bytes: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        note: "recommended: multilingual, accurate, fast with Metal",
    },
    KnownModel {
        name: "large-v3-turbo",
        bytes: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        note: "unquantized turbo; slightly better, three times the size",
    },
    KnownModel {
        name: "small.en",
        bytes: 487_614_201,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        note: "English only; for Intel Macs or a busy machine",
    },
    KnownModel {
        name: "base.en",
        bytes: 147_964_211,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        note: "English only; quick to try, noticeably less accurate",
    },
];

impl KnownModel {
    /// Where the file is published.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            model_file_name(self.name)
        )
    }
}

/// A known model by name, with or without the `ggml-` prefix and `.bin`.
#[must_use]
pub fn known_model(name: &str) -> Option<&'static KnownModel> {
    let name = name.strip_prefix("ggml-").unwrap_or(name);
    let name = name.strip_suffix(".bin").unwrap_or(name);
    KNOWN_MODELS.iter().find(|m| m.name == name)
}

/// `large-v3-turbo-q5_0` → `ggml-large-v3-turbo-q5_0.bin`.
#[must_use]
pub fn model_file_name(name: &str) -> String {
    format!("ggml-{name}.bin")
}

/// Where downloaded models are kept: beside the library, not inside it.
#[must_use]
pub fn models_dir(data_root: &Path) -> PathBuf {
    data_root.join("models")
}

/// Why a model file was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// Nothing at the path.
    #[error("no model at {0}")]
    Missing(PathBuf),
    /// A known model's file that is not the published one: a partial download,
    /// or not the file it is named for.
    #[error("{path} is not the published {name} file ({reason}); download it again")]
    Mismatch {
        /// The file.
        path: PathBuf,
        /// The model it is named for.
        name: &'static str,
        /// What differed.
        reason: String,
    },
    /// The file could not be read.
    #[error("could not read {0}: {1}")]
    Unreadable(PathBuf, String),
}

/// Check that `path` holds a model.
///
/// A file named for one of [`KNOWN_MODELS`] must match its published size and
/// SHA-256; any other file is accepted as the user's own and returns `None`.
/// whisper.cpp parses whatever it is given, so the check is also what keeps a
/// truncated download from reaching it.
///
/// # Errors
///
/// When the file is missing, unreadable, or not the published file.
pub fn verify_model(path: &Path) -> Result<Option<&'static KnownModel>, ModelError> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ModelError::Missing(path.to_owned()),
        _ => ModelError::Unreadable(path.to_owned(), e.to_string()),
    })?;
    if !meta.is_file() {
        return Err(ModelError::Missing(path.to_owned()));
    }
    let Some(known) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(known_model)
    else {
        return Ok(None);
    };
    if meta.len() != known.bytes {
        return Err(ModelError::Mismatch {
            path: path.to_owned(),
            name: known.name,
            reason: format!("{} bytes, expected {}", meta.len(), known.bytes),
        });
    }
    let digest = sha256_file(path).map_err(|e| ModelError::Unreadable(path.to_owned(), e))?;
    if digest != known.sha256 {
        return Err(ModelError::Mismatch {
            path: path.to_owned(),
            name: known.name,
            reason: "checksum differs".to_owned(),
        });
    }
    Ok(Some(known))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Transcription on this machine, for one session.
///
/// The model is not opened here: it loads on the first leg's worker once
/// capture is up, so `Start` is not held for it, and a missing or unreadable
/// file arrives as that leg's error on the dashboard rather than a refusal to
/// record.
#[cfg(target_os = "macos")]
#[must_use]
pub fn local_transcription(
    settings: &SttSettings,
    data_root: &Path,
    session_id: &str,
    mic_present: bool,
    mic_enabled: bool,
) -> Transcription {
    let engine = fotw_stt::WhisperEngine::new(settings.model_path(data_root), settings.language());
    Transcription::Local(crate::session::LocalLegs::for_session(
        std::sync::Arc::new(engine),
        session_id,
        mic_present,
        mic_enabled,
    ))
}

/// Transcription on this machine: not built for this platform yet, so a
/// recording made with it is kept untranscribed.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn local_transcription(
    _settings: &SttSettings,
    _data_root: &Path,
    _session_id: &str,
    _mic_present: bool,
    _mic_enabled: bool,
) -> Transcription {
    crate::diag!("  ! local transcription is macOS-only for now; recording without a transcript");
    Transcription::Disabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_row_is_deepgram_and_the_default_model() {
        let settings: SttSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.engine, SttEngine::Deepgram);
        assert_eq!(
            settings.model_path(Path::new("/data")),
            Path::new("/data/models/ggml-large-v3-turbo-q5_0.bin")
        );
        assert_eq!(settings.language(), None);
    }

    #[test]
    fn the_engine_round_trips_as_a_lowercase_word() {
        let settings = SttSettings {
            engine: SttEngine::Local,
            model: "/m/ggml-base.en.bin".into(),
            language: "EN".into(),
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""engine":"local""#), "{json}");
        let back: SttSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
        assert_eq!(back.language().as_deref(), Some("en"));
    }

    #[test]
    fn the_choice_is_stored_in_the_library() {
        let mut db = Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x33; 32])).unwrap();
        assert_eq!(SttSettings::read(&db), SttSettings::default());
        let local = SttSettings {
            engine: SttEngine::Local,
            model: "/m/ggml-small.en.bin".into(),
            language: String::new(),
        };
        local.write(&mut db).unwrap();
        assert_eq!(SttSettings::read(&db), local);
    }

    #[test]
    fn auto_means_detect() {
        let settings = SttSettings {
            language: "auto".into(),
            ..SttSettings::default()
        };
        assert_eq!(settings.language(), None);
    }

    #[test]
    fn known_models_are_found_by_any_spelling() {
        for name in [
            "large-v3-turbo-q5_0",
            "ggml-large-v3-turbo-q5_0",
            "ggml-large-v3-turbo-q5_0.bin",
        ] {
            assert_eq!(known_model(name).unwrap().name, DEFAULT_MODEL);
        }
        assert!(known_model("large-v3").is_none());
        assert!(known_model(DEFAULT_MODEL).is_some());
        assert!(
            KNOWN_MODELS
                .iter()
                .all(|m| m.sha256.len() == 64 && m.url().ends_with(".bin"))
        );
    }

    #[test]
    fn a_missing_model_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggml-base.en.bin");
        assert_eq!(verify_model(&path), Err(ModelError::Missing(path.clone())));
    }

    #[test]
    fn a_truncated_known_model_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggml-base.en.bin");
        std::fs::write(&path, b"partial").unwrap();
        assert!(matches!(
            verify_model(&path),
            Err(ModelError::Mismatch {
                name: "base.en",
                ..
            })
        ));
    }

    #[test]
    fn an_unknown_model_is_the_users_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggml-my-finetune.bin");
        std::fs::write(&path, b"weights").unwrap();
        assert_eq!(verify_model(&path), Ok(None));
    }

    #[test]
    fn sha256_matches_the_standard_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

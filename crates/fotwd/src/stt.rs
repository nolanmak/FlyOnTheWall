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
    /// Explicit Deepgram preference with a prepared on-device backup.
    Fallback,
}

/// The stored engine choice.
///
/// Only a missing settings row uses the fresh-library default. An unreadable
/// or invalid saved choice must never silently enable cloud transcription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SttSettings {
    /// Which engine.
    pub engine: SttEngine,
    /// The ggml model file the local engine loads, as an absolute path.
    #[serde(default)]
    pub model: String,
    /// ISO 639-1 code for the local engine. Empty detects per utterance.
    #[serde(default)]
    pub language: String,
}

impl SttSettings {
    /// The settings in force for this library.
    pub fn read(db: &Db) -> Result<Self, String> {
        match db.get_setting(SETTINGS_KEY).map_err(|e| e.to_string())? {
            None => Ok(Self::default()),
            Some(saved) => serde_json::from_str(&saved).map_err(|_| {
                "Saved transcription settings are invalid; select a transcription engine again"
                    .into()
            }),
        }
    }

    /// Prepare settings for an explicit user engine selection. Valid stored
    /// model/language choices survive; invalid settings can be repaired by
    /// selecting an engine again. This never persists or starts transcription.
    #[must_use]
    pub fn for_selection(db: &Db, engine: SttEngine) -> Self {
        let mut settings = Self::read(db).unwrap_or_default();
        settings.engine = engine;
        settings
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
        let code = self.language.trim().to_ascii_lowercase();
        (!code.is_empty() && code != "auto").then_some(code)
    }
}

/// Build an owned, deferred admission. No capture or cloud connection may
/// begin until the session resolves this preparation successfully.
pub fn fallback_transcription(
    settings: SttSettings,
    data_root: PathBuf,
    cloud: Box<dyn FnOnce() -> Result<crate::session::DeepgramLegs, String> + Send>,
) -> Transcription {
    Transcription::PreparingFailover(Box::pin(async move {
        let (engine, revision) =
            tokio::task::spawn_blocking(move || pinned_local_model(&settings, &data_root))
                .await
                .map_err(|_| "local backup model preparation failed".to_owned())??;
        let legs = tokio::task::spawn_blocking(cloud)
            .await
            .map_err(|_| "could not read the Deepgram credential".to_owned())??;
        let config = |cloud| {
            let mut config =
                crate::failover::stream::FailoverConfig::new(cloud, std::sync::Arc::clone(&engine));
            config.local_revision = Some(revision.clone());
            config
        };
        crate::session::FailoverLegs::prepare(
            config(*legs.system),
            legs.mic.map(|mic| config(*mic)),
        )
        .await
        .map_err(|e| e.message)
    }))
}

#[cfg(target_os = "macos")]
fn pinned_local_model(
    settings: &SttSettings,
    data_root: &Path,
) -> Result<(std::sync::Arc<dyn fotw_stt::RecognizerFactory>, String), String> {
    let path = settings.model_path(data_root);
    if !path.is_absolute() {
        return Err(
            "local backup model path must be absolute; select it again with `fotwd stt fallback`"
                .into(),
        );
    }
    let known = verify_model(&path).map_err(|e| e.to_string())?;
    let digest = match known {
        Some(model) => model.sha256.to_owned(),
        None => sha256_file(&path)?,
    };
    let engine = fotw_stt::WhisperEngine::new(&path, settings.language());
    engine.load().map_err(|e| e.message)?;
    // The context must correspond to the revision we will promise on restart.
    if sha256_file(&path)? != digest {
        return Err("local backup model changed during preparation; select it again".into());
    }
    Ok((std::sync::Arc::new(engine), format!("sha256:{digest}")))
}

#[cfg(target_os = "macos")]
pub(crate) fn saved_local_model(
    plan: &crate::failover::coverage::LocalPlan,
    candidate: &Path,
    data_root: &Path,
) -> Result<std::sync::Arc<dyn fotw_stt::RecognizerFactory>, String> {
    use fotw_stt::RecognizerFactory;
    plan.validate().map_err(|e| e.to_string())?;
    let digest = plan
        .engine
        .revision
        .as_deref()
        .and_then(|r| r.strip_prefix("sha256:"))
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| "Saved model has no verified SHA-256 revision; audio retained".to_owned())?;
    let managed = known_model(&plan.engine.model)
        .map(|m| models_dir(data_root).join(model_file_name(m.name)));
    let path = [Some(candidate.to_owned()), managed]
        .into_iter()
        .flatten()
        .find(|path| {
            path.is_absolute()
                && fotw_stt::WhisperEngine::new(path, plan.language.clone()).model()
                    == plan.engine.model
                && sha256_file(path).is_ok_and(|hash| hash == digest)
        })
        .ok_or_else(|| {
            "Install the original model revision before retrying; audio retained".to_owned()
        })?;
    let engine = fotw_stt::WhisperEngine::new(&path, plan.language.clone());
    if engine.model() != plan.engine.model {
        return Err("Installed model identity differs from saved audio".into());
    }
    // Native loading is deferred to create(), after the backend has obtained
    // exclusive source/journal ownership. The wrapper verifies the context's
    // bytes before and after that load, without reading any cloud credential.
    Ok(std::sync::Arc::new(SavedFactory {
        engine,
        path,
        digest: digest.into(),
    }))
}

#[cfg(target_os = "macos")]
struct SavedFactory {
    engine: fotw_stt::WhisperEngine,
    path: PathBuf,
    digest: String,
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn saved_model_resolution_accepts_a_verified_configured_copy_beside_a_stale_managed_copy() {
    let root = tempfile::tempdir().unwrap();
    let candidate = root.path().join("configured/ggml-small.en.bin");
    let managed = models_dir(root.path()).join(model_file_name("small.en"));
    std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(
        &candidate,
        b"metadata-only fixture; never load as a native model",
    )
    .unwrap();
    std::fs::write(managed, b"unfinished managed download").unwrap();
    let plan = crate::failover::coverage::LocalPlan {
        engine: crate::failover::coverage::EngineDescriptor {
            provider: fotw_stt::local::PROVIDER.into(),
            model: "small.en".into(),
            revision: Some(format!("sha256:{}", sha256_file(&candidate).unwrap())),
            capabilities: fotw_stt::SttCapabilities::deepgram(),
        },
        language: Some("en".into()),
        endpoint: Default::default(),
    };
    // Resolver construction checks identity/revision; create() is deliberately
    // not called, so these fake bytes never enter whisper.cpp.
    let factory = saved_local_model(&plan, &candidate, root.path()).unwrap();
    assert_eq!(factory.model(), "small.en");
    assert_eq!(factory.language(), Some("en"));
}
#[cfg(target_os = "macos")]
impl fotw_stt::RecognizerFactory for SavedFactory {
    fn model(&self) -> &str {
        self.engine.model()
    }
    fn language(&self) -> Option<&str> {
        self.engine.language()
    }
    fn create(&self) -> Result<Box<dyn fotw_stt::Recognizer>, fotw_stt::SttError> {
        let verify = || {
            if sha256_file(&self.path).is_ok_and(|hash| hash == self.digest) {
                Ok(())
            } else {
                Err(fotw_stt::SttError::new(
                    fotw_stt::SttErrorClass::Unsupported,
                    fotw_stt::local::PROVIDER,
                    "Saved model revision changed; audio retained",
                ))
            }
        };
        verify()?;
        let state = self.engine.create()?;
        verify()?;
        Ok(state)
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn saved_local_model(
    _plan: &crate::failover::coverage::LocalPlan,
    _candidate: &Path,
    _data_root: &Path,
) -> Result<std::sync::Arc<dyn fotw_stt::RecognizerFactory>, String> {
    Err("Saved local transcription recovery requires a supported local runtime".into())
}

#[cfg(not(target_os = "macos"))]
fn pinned_local_model(
    _settings: &SttSettings,
    _data_root: &Path,
) -> Result<(std::sync::Arc<dyn fotw_stt::RecognizerFactory>, String), String> {
    Err("local backup transcription is available only on macOS; select Deepgram-only or a supported local runtime".into())
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

/// Immutable publisher revision for the catalog, including its MIT model card.
pub const MODEL_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// The models `fotwd stt local` knows by name, from
/// `huggingface.co/ggerganov/whisper.cpp`.
pub const KNOWN_MODELS: &[KnownModel] = &[
    KnownModel {
        name: "large-v3-turbo-q5_0",
        bytes: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        note: "multilingual; Q5_0 quantized weights",
    },
    KnownModel {
        name: "large-v3-turbo",
        bytes: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        note: "multilingual; unquantized weights",
    },
    KnownModel {
        name: "small.en",
        bytes: 487_614_201,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        note: "English only; smaller weights",
    },
    KnownModel {
        name: "base.en",
        bytes: 147_964_211,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        note: "English only; smallest catalog download",
    },
];

impl KnownModel {
    /// Where the file is published.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/{MODEL_REVISION}/{}",
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
/// File identity is resolved on an owned preparation task. Native loading stays
/// on the source worker after capture begins. An invalid/unavailable model still
/// records audio and reports its error, with no implicit cloud fallback.
#[cfg(target_os = "macos")]
#[must_use]
pub fn local_transcription(
    settings: &SttSettings,
    data_root: &Path,
    session_id: &str,
    mic_present: bool,
    mic_enabled: bool,
) -> Transcription {
    let settings = settings.clone();
    let data_root = data_root.to_path_buf();
    let session_id = session_id.to_owned();
    Transcription::PreparingLocal(Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            use fotw_stt::RecognizerFactory;
            let path = settings.model_path(&data_root);
            let engine = fotw_stt::WhisperEngine::new(&path, settings.language());
            let expected_revision =
                known_model(engine.model()).map(|model| format!("sha256:{}", model.sha256));
            let checked = verify_model(&path)
                .map_err(|e| e.to_string())
                .and_then(|known| match known {
                    Some(model) => Ok(model.sha256.to_owned()),
                    None => sha256_file(&path),
                });
            let (factory, revision): (std::sync::Arc<dyn RecognizerFactory>, Option<String>) =
                match checked {
                    Ok(digest) => (
                        std::sync::Arc::new(SavedFactory {
                            engine,
                            path,
                            digest: digest.clone(),
                        }),
                        Some(format!("sha256:{digest}")),
                    ),
                    Err(message) => (
                        std::sync::Arc::new(UnavailableLocal {
                            model: engine.model().to_owned(),
                            language: engine.language().map(str::to_owned),
                            message,
                        }),
                        expected_revision,
                    ),
                };
            let mut legs = crate::session::LocalLegs::for_session(
                factory,
                &session_id,
                mic_present,
                mic_enabled,
            );
            legs.revision = revision;
            legs
        })
        .await
        .map_err(|_| "local model identity preparation failed".to_owned())
    }))
}

#[cfg(target_os = "macos")]
struct UnavailableLocal {
    model: String,
    language: Option<String>,
    message: String,
}

#[cfg(all(test, target_os = "macos"))]
#[tokio::test]
async fn unavailable_catalog_model_keeps_its_expected_identity_for_later_recovery() {
    let root = tempfile::tempdir().unwrap();
    let settings = SttSettings {
        engine: SttEngine::Local,
        model: root
            .path()
            .join("ggml-small.en.bin")
            .to_string_lossy()
            .into_owned(),
        language: "en".into(),
    };
    let prepared = local_transcription(&settings, root.path(), "fixture", false, false)
        .prepare(&crate::session::SessionControl::new())
        .await
        .unwrap();
    let Transcription::Local(legs) = prepared else {
        panic!("expected recording-capable local configuration");
    };
    let expected = format!("sha256:{}", known_model("small.en").unwrap().sha256);
    assert_eq!(legs.revision.as_deref(), Some(expected.as_str()));
    assert!(
        legs.engine.create().is_err(),
        "an expected identity must not pretend the model is available"
    );
}

#[cfg(all(test, target_os = "macos"))]
#[tokio::test]
async fn local_identity_is_pinned_without_loading_native_weights_and_refuses_a_changed_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("custom-model.ggml");
    std::fs::write(
        &path,
        b"metadata-only identity fixture; never native weights",
    )
    .unwrap();
    let settings = SttSettings {
        engine: SttEngine::Local,
        model: path.to_string_lossy().into_owned(),
        language: "en".into(),
    };
    let expected = format!("sha256:{}", sha256_file(&path).unwrap());
    let prepared = local_transcription(&settings, root.path(), "fixture", false, false)
        .prepare(&crate::session::SessionControl::new())
        .await
        .unwrap();
    let Transcription::Local(legs) = prepared else {
        panic!("expected local configuration");
    };
    assert_eq!(legs.revision.as_deref(), Some(expected.as_str()));
    std::fs::write(&path, b"changed before native allocation").unwrap();
    let error = legs
        .engine
        .create()
        .err()
        .expect("changed identity must be refused before native initialization");
    assert!(error.message.contains("revision changed"));
}
#[cfg(target_os = "macos")]
impl fotw_stt::RecognizerFactory for UnavailableLocal {
    fn model(&self) -> &str {
        &self.model
    }
    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
    fn create(&self) -> Result<Box<dyn fotw_stt::Recognizer>, fotw_stt::SttError> {
        Err(fotw_stt::SttError::new(
            fotw_stt::SttErrorClass::Unsupported,
            fotw_stt::local::PROVIDER,
            self.message.clone(),
        ))
    }
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
    fn fallback_choice_survives_restart_without_changing_local_settings() {
        let mut db = Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x38; 32])).unwrap();
        let settings = SttSettings {
            engine: SttEngine::Fallback,
            model: "/fixture/ggml-small.en.bin".into(),
            language: "en".into(),
        };
        settings.write(&mut db).unwrap();
        assert_eq!(SttSettings::read(&db).unwrap(), settings);
        assert_eq!(SttSettings::default().engine, SttEngine::Deepgram);
    }

    #[test]
    fn an_explicit_engine_can_omit_optional_model_and_language() {
        let settings: SttSettings = serde_json::from_str(r#"{"engine":"deepgram"}"#).unwrap();
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
        assert_eq!(SttSettings::read(&db).unwrap(), SttSettings::default());
        let local = SttSettings {
            engine: SttEngine::Local,
            model: "/m/ggml-small.en.bin".into(),
            language: String::new(),
        };
        local.write(&mut db).unwrap();
        assert_eq!(SttSettings::read(&db).unwrap(), local);
    }

    #[test]
    fn malformed_or_unknown_saved_choices_cannot_revert_to_cloud() {
        let mut db = Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x34; 32])).unwrap();
        for saved in [
            "{",
            "null",
            "[]",
            "{}",
            r#"{"engine":"local","model":123}"#,
            r#"{"engine":"future-local-engine"}"#,
            r#"{"model":"/models/ggml-small.en.bin"}"#,
            r#"{"engine":"deepgram","unknown_privacy_policy":"local-only"}"#,
        ] {
            db.put_setting(SETTINGS_KEY, saved).unwrap();
            assert!(
                SttSettings::read(&db).is_err(),
                "invalid saved choice must be surfaced: {saved}"
            );
            assert_eq!(
                db.get_setting(SETTINGS_KEY).unwrap().as_deref(),
                Some(saved)
            );
        }
    }

    #[test]
    fn repairing_invalid_settings_requires_an_explicit_engine_choice() {
        let mut db = Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x35; 32])).unwrap();
        db.put_setting(SETTINGS_KEY, "{").unwrap();
        assert_eq!(
            SttSettings::for_selection(&db, SttEngine::Local).engine,
            SttEngine::Local
        );
        assert!(
            SttSettings::read(&db).is_err(),
            "preparing a choice does not silently rewrite the row"
        );
        let explicit = SttSettings::for_selection(&db, SttEngine::Local);
        explicit.write(&mut db).unwrap();
        assert_eq!(SttSettings::read(&db).unwrap().engine, SttEngine::Local);
    }

    #[test]
    fn auto_means_detect() {
        for language in ["auto", " AUTO ", "", "  "] {
            let settings = SttSettings {
                language: language.into(),
                ..SttSettings::default()
            };
            assert_eq!(settings.language(), None);
        }
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

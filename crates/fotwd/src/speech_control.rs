//! Dashboard speech settings: model work runs outside the library mutex.
use crate::stt::{SETTINGS_KEY, SttSettings};
use fotw_store::Db;
use fotw_web::speech::{
    ModelAvailability, SpeechControl, SpeechMode, SpeechSettingsDoc, SpeechSettingsResponse,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type ModelCheck = dyn Fn(&SttSettings, &Path) -> Result<(), String> + Send + Sync;

/// Owns a separate library handle. Never reads credentials or opens capture.
pub struct SpeechSettingsControl {
    db: Arc<Mutex<Db>>,
    data_root: PathBuf,
    check: Arc<ModelCheck>,
}
impl std::fmt::Debug for SpeechSettingsControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpeechSettingsControl(<redacted>)")
    }
}
impl SpeechSettingsControl {
    /// Model validation is deferred until an explicit local-policy save.
    pub fn new(db: Db, data_root: PathBuf) -> Self {
        Self {
            db: Arc::new(Mutex::new(db)),
            data_root,
            check: Arc::new(check_model),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl SpeechControl for SpeechSettingsControl {
    fn read(&self) -> SpeechSettingsResponse {
        let settings = SttSettings::read(&self.lock());
        let result = settings.and_then(|saved| {
            let doc: SpeechSettingsDoc =
                serde_json::from_value(serde_json::to_value(&saved).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let (path, present) = if saved.uses_parakeet() {
                (
                    crate::speechd::models_root(&self.data_root)
                        .join(crate::speechd::ENGINE)
                        .join(crate::speechd::REVISION),
                    crate::speechd::present(&self.data_root),
                )
            } else {
                let path = saved.model_path(&self.data_root);
                let present = path.is_file();
                (path, present)
            };
            let availability = if doc.engine == SpeechMode::Deepgram {
                ModelAvailability::NotRequired
            } else if !cfg!(target_os = "macos") {
                ModelAvailability::Unsupported
            } else if present && crate::stt::vad_model_path(&self.data_root).is_file() {
                ModelAvailability::PresentUnchecked
            } else {
                ModelAvailability::Missing
            };
            let model_path =
                (doc.engine != SpeechMode::Deepgram).then(|| path.to_string_lossy().into_owned());
            Ok(SpeechSettingsResponse {
                settings: Some(doc),
                model_availability: availability,
                model_path,
                error: None,
                captions_installed: crate::speechd::present_model(
                    &self.data_root,
                    &crate::speechd::CAPTIONS,
                ),
            })
        });
        result.unwrap_or_else(|error| SpeechSettingsResponse {
            settings: None,
            model_availability: ModelAvailability::Unknown,
            model_path: None,
            error: Some(error),
            captions_installed: false,
        })
    }
    fn set(&self, doc: SpeechSettingsDoc) -> Result<(), String> {
        let mut doc = doc.normalized()?;
        if doc.engine != SpeechMode::Deepgram && crate::speechd::is_engine(&doc.model) {
            if !doc.language.is_empty() && doc.language != "en" {
                return Err("Parakeet Unified transcribes English only; use en or auto.".into());
            }
            doc.model = crate::speechd::ENGINE.into();
        } else if doc.engine != SpeechMode::Deepgram && !doc.model.is_empty() {
            let path = if !doc.model.contains('/') {
                crate::stt::known_model(&doc.model).map(|m| {
                    crate::stt::models_dir(&self.data_root)
                        .join(crate::stt::model_file_name(m.name))
                })
            } else {
                None
            }
            .unwrap_or_else(|| PathBuf::from(&doc.model));
            if !path.is_absolute() {
                return Err("Choose an installed model name or an absolute model path.".into());
            }
            doc.model = path.to_string_lossy().into_owned();
        }
        let encoded = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
        let settings: SttSettings = serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
        // Native loading can be slow. No database lock spans it, so reads and
        // recording controls stay responsive and cancelled HTTP saves hold no DB.
        if doc.engine != SpeechMode::Deepgram {
            (self.check)(&settings, &self.data_root)?;
        }
        self.lock()
            .put_setting(SETTINGS_KEY, &encoded)
            .map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "macos")]
fn check_model(settings: &SttSettings, root: &Path) -> Result<(), String> {
    if settings.uses_parakeet() {
        return check_parakeet(
            root,
            &crate::stt::VAD_MODEL,
            crate::speechd::Launch::bundled(),
        );
    }
    let path = settings.model_path(root);
    crate::stt::verify_model(&path).map_err(|e| e.to_string())?;
    let vad = crate::stt::verify_vad_model(root)?;
    fotw_stt::WhisperEngine::new(path, settings.language())
        .with_speech_gate(vad)
        .load()
        .map_err(|e| e.message)
}
#[cfg(target_os = "macos")]
fn check_parakeet(
    root: &Path,
    detector: &'static crate::stt::KnownModel,
    launch: Result<crate::speechd::Launch, String>,
) -> Result<(), String> {
    // Cheap on purpose: a save may come during a recording, and loading a
    // second ~600 MB model would compete with the live legs. Start loads it
    // and refuses with the same hint if it does not load.
    crate::stt::verify_detector(root, detector)?;
    launch?;
    if crate::speechd::present(root) {
        Ok(())
    } else {
        Err(format!(
            "The {} speech model is not installed. {}.",
            crate::speechd::ENGINE,
            fotw_stt::speechd::install_hint(crate::speechd::ENGINE)
        ))
    }
}
#[cfg(not(target_os = "macos"))]
fn check_model(_: &SttSettings, _: &Path) -> Result<(), String> {
    Err("Local speech recognition is currently supported only on macOS.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn control() -> SpeechSettingsControl {
        SpeechSettingsControl::new(
            Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x41; 32])).unwrap(),
            PathBuf::from("/fixture"),
        )
    }
    fn doc(engine: SpeechMode) -> SpeechSettingsDoc {
        SpeechSettingsDoc {
            engine,
            model: "small.en".into(),
            language: "en".into(),
            live_captions: true,
        }
    }
    #[test]
    fn invalid_saved_choice_is_reported_without_guessing_cloud() {
        let c = control();
        c.lock().put_setting(SETTINGS_KEY, "{").unwrap();
        let response = c.read();
        assert!(response.settings.is_none());
        assert!(response.error.is_some());
        assert_eq!(response.model_availability, ModelAvailability::Unknown);
    }
    #[test]
    fn refused_model_save_preserves_previous_local_policy() {
        let mut c = control();
        c.check = Arc::new(|_, _| Ok(()));
        c.set(doc(SpeechMode::Local)).unwrap();
        let before = c.read().settings;
        c.check = Arc::new(|_, _| Err("fixture invalid model/language".into()));
        assert!(c.set(doc(SpeechMode::Fallback)).is_err());
        assert_eq!(c.read().settings, before);
    }
    #[test]
    fn parakeet_is_saved_by_name_and_reported_missing_until_installed() {
        let mut c = control();
        c.check = Arc::new(|settings, _| {
            assert!(settings.uses_parakeet());
            Ok(())
        });
        let mut parakeet = doc(SpeechMode::Local);
        parakeet.model = " parakeet-unified ".into();
        c.set(parakeet.clone()).unwrap();
        let read = c.read();
        assert_eq!(read.settings.unwrap().model, crate::speechd::ENGINE);
        // Parakeet runs only on macOS; elsewhere the dashboard says local
        // speech is unsupported rather than asking for an install.
        let expected = if cfg!(target_os = "macos") {
            ModelAvailability::Missing
        } else {
            ModelAvailability::Unsupported
        };
        assert_eq!(read.model_availability, expected);
        assert!(
            read.model_path
                .unwrap()
                .contains("speechd/parakeet-unified/")
        );
        parakeet.language = "de".into();
        assert!(c.set(parakeet).unwrap_err().contains("English only"));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn saving_parakeet_checks_its_files_without_starting_a_helper() {
        let root = tempfile::tempdir().unwrap();
        let detector = crate::stt::fixture_detector(root.path());
        // A program that does not exist: any attempt to start it fails.
        let no_spawn = || {
            Ok(crate::speechd::Launch {
                program: "/nonexistent/fotw-speechd".into(),
                extra_args: Vec::new(),
            })
        };
        let missing = check_parakeet(root.path(), detector, no_spawn()).unwrap_err();
        assert!(
            missing.contains("fotwd stt models install parakeet-unified"),
            "{missing}"
        );
        std::fs::create_dir_all(
            crate::speechd::models_root(root.path())
                .join(crate::speechd::ENGINE)
                .join(crate::speechd::REVISION),
        )
        .unwrap();
        assert_eq!(check_parakeet(root.path(), detector, no_spawn()), Ok(()));
        assert!(
            check_parakeet(root.path(), detector, Err("no helper in this build".into()))
                .unwrap_err()
                .contains("no helper")
        );
    }
    #[test]
    fn a_fresh_library_shows_parakeet_as_the_local_model() {
        let c = control();
        assert_eq!(c.read().settings.unwrap().model, crate::speechd::ENGINE);
    }
    #[test]
    fn cloud_save_never_loads_a_local_model() {
        let mut c = control();
        c.check = Arc::new(|_, _| panic!("cloud-only needs no model"));
        let mut cloud = doc(SpeechMode::Deepgram);
        cloud.model = "old-unavailable-model".into();
        c.set(cloud).unwrap();
        assert_eq!(c.read().model_availability, ModelAvailability::NotRequired);
    }
    #[test]
    fn local_model_validation_does_not_hold_database_mutex() {
        let mut c = control();
        let database = c.db.clone();
        c.check = Arc::new(move |_, _| {
            assert!(
                database.try_lock().is_ok(),
                "model loading held the library mutex"
            );
            Ok(())
        });
        c.set(doc(SpeechMode::Local)).unwrap();
        assert_eq!(
            c.read().settings.unwrap().model,
            "/fixture/models/ggml-small.en.bin"
        );
    }
    #[test]
    fn fallback_policy_and_language_survive_dashboard_save() {
        let mut c = control();
        c.check = Arc::new(|settings, _| {
            assert_eq!(settings.language().as_deref(), Some("en"));
            Ok(())
        });
        c.set(doc(SpeechMode::Fallback)).unwrap();
        assert_eq!(c.read().settings.unwrap().engine, SpeechMode::Fallback);
    }
}

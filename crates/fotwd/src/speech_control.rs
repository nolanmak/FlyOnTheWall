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
            let path = saved.model_path(&self.data_root);
            let availability = if doc.engine == SpeechMode::Deepgram {
                ModelAvailability::NotRequired
            } else if !cfg!(target_os = "macos") {
                ModelAvailability::Unsupported
            } else if path.is_file() && crate::stt::vad_model_path(&self.data_root).is_file() {
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
            })
        });
        result.unwrap_or_else(|error| SpeechSettingsResponse {
            settings: None,
            model_availability: ModelAvailability::Unknown,
            model_path: None,
            error: Some(error),
        })
    }
    fn set(&self, doc: SpeechSettingsDoc) -> Result<(), String> {
        let mut doc = doc.normalized()?;
        if doc.engine != SpeechMode::Deepgram && !doc.model.is_empty() {
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
    let path = settings.model_path(root);
    crate::stt::verify_model(&path).map_err(|e| e.to_string())?;
    let vad = crate::stt::verify_vad_model(root)?;
    fotw_stt::WhisperEngine::new(path, settings.language())
        .with_speech_gate(vad)
        .load()
        .map_err(|e| e.message)
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

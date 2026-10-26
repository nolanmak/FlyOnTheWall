//! Encrypted saved system-audio policy. Never reads provider credentials.
use fotw_store::Db;
use fotw_web::capture::CaptureSettingsDoc;

/// Versioned encrypted-library row; absence preserves legacy capture.
pub const SETTINGS_KEY: &str = "capture.exclusions.v1";

/// Refuse malformed saved policy rather than silently broadening capture.
///
/// # Errors
/// Database access or invalid persisted JSON.
pub fn read(db: &Db) -> Result<CaptureSettingsDoc, String> {
    match db.get_setting(SETTINGS_KEY).map_err(|e| e.to_string())? {
        None => Ok(CaptureSettingsDoc::default()),
        Some(raw) => serde_json::from_str(&raw).map_err(|_| {
            "Saved audio exclusions are invalid. Repair them before recording.".into()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> Db {
        Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x31; 32])).unwrap()
    }
    #[test]
    fn missing_row_preserves_legacy_capture_but_corruption_never_widens_scope() {
        let mut db = db();
        assert_eq!(read(&db).unwrap(), CaptureSettingsDoc::default());
        for raw in ["broken", "{}", r#"{"ignore_spotify":true}"#] {
            db.put_setting(SETTINGS_KEY, raw).unwrap();
            assert!(read(&db).is_err());
        }
    }
    #[test]
    fn both_switches_round_trip_without_changing_each_other() {
        let mut db = db();
        for ignore_spotify in [false, true] {
            for ignore_brave in [false, true] {
                let choice = CaptureSettingsDoc {
                    ignore_spotify,
                    ignore_brave,
                };
                db.put_setting(SETTINGS_KEY, &serde_json::to_string(&choice).unwrap())
                    .unwrap();
                assert_eq!(read(&db).unwrap(), choice);
            }
        }
    }
}

/// Resolve exactly one immutable recording scope, including installed helpers.
/// Native recovery must reuse this scope rather than reread mutable settings.
pub fn scope(
    settings: CaptureSettingsDoc,
    supported: bool,
    mut family: impl FnMut(&str) -> Result<Vec<fotw_audio::AppRef>, fotw_audio::TapError>,
) -> Result<fotw_audio::SystemScope, String> {
    if !settings.filtered() {
        return Ok(fotw_audio::SystemScope::DefaultOutputMix);
    }
    if !supported {
        return Err("Audio exclusions require macOS 26 or newer. Turn them off explicitly to record the full system mix.".into());
    }
    let mut excluded = Vec::new();
    for (selected, id) in [
        (settings.ignore_spotify, "com.spotify.client"),
        (settings.ignore_brave, "com.brave.Browser"),
    ] {
        if selected {
            let identities = family(id).map_err(|e| e.to_string())?;
            if identities.is_empty() {
                return Err("Application exclusion identities could not be confirmed; recording was refused.".into());
            }
            excluded.extend(identities);
        }
    }
    Ok(fotw_audio::SystemScope::AllExcept(excluded))
}

/// Read persisted policy once before any device opens.
pub fn configured(root: &std::path::Path) -> Result<fotw_audio::SystemScope, String> {
    let settings = read(&crate::open_library(root)?)?;
    scope(
        settings,
        fotw_audio::platform::application_exclusions_supported(),
        fotw_audio::platform::application_exclusion_family,
    )
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use fotw_audio::{AppRef, SystemScope, TapError};
    #[test]
    fn disabled_policy_never_queries_installed_apps_or_requires_capability() {
        assert_eq!(
            scope(CaptureSettingsDoc::default(), false, |_| panic!(
                "no app selected"
            ))
            .unwrap(),
            SystemScope::DefaultOutputMix
        );
    }
    #[test]
    fn selected_apps_and_helpers_remain_an_immutable_scope_after_preferences_change() {
        let mut prefs = CaptureSettingsDoc {
            ignore_spotify: true,
            ignore_brave: true,
        };
        let frozen = scope(prefs, true, |id| {
            Ok(vec![
                AppRef::BundleId(id.into()),
                AppRef::BundleId(format!("{id}.helper")),
            ])
        })
        .unwrap();
        prefs.ignore_brave = false;
        let next = scope(prefs, true, |id| Ok(vec![AppRef::BundleId(id.into())])).unwrap();
        let SystemScope::AllExcept(ids) = frozen else {
            panic!("must be filtered")
        };
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&AppRef::BundleId("com.brave.Browser.helper".into())));
        assert!(!ids.contains(&AppRef::BundleId("com.google.Chrome".into())));
        assert_eq!(
            next,
            SystemScope::AllExcept(vec![AppRef::BundleId("com.spotify.client".into())])
        );
    }
    #[test]
    fn unsupported_missing_or_unresolved_exclusions_never_return_the_full_mix() {
        let prefs = CaptureSettingsDoc {
            ignore_spotify: true,
            ignore_brave: false,
        };
        assert!(scope(prefs, false, |_| panic!("unsupported")).is_err());
        assert!(scope(prefs, true, |_| Err(TapError::unsupported("missing app"))).is_err());
        assert!(scope(prefs, true, |_| Ok(vec![])).is_err());
    }
}

/// HTTP control with its own encrypted-library connection.
pub struct SettingsControl {
    db: std::sync::Mutex<Db>,
    supported: bool,
}
impl std::fmt::Debug for SettingsControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CaptureSettingsControl(<redacted>)")
    }
}
impl SettingsControl {
    /// Does not open capture, launch apps or read provider credentials.
    pub fn new(db: Db) -> Self {
        Self {
            db: std::sync::Mutex::new(db),
            supported: fotw_audio::platform::application_exclusions_supported(),
        }
    }
}
impl fotw_web::capture::CaptureControl for SettingsControl {
    fn read(&self) -> fotw_web::capture::CaptureSettingsResponse {
        let result = read(
            &self
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let (settings, error) = match result {
            Ok(doc) => (Some(doc), None),
            Err(error) => (None, Some(error)),
        };
        fotw_web::capture::CaptureSettingsResponse {
            settings,
            supported: self.supported,
            error,
        }
    }
    fn set(&self, doc: CaptureSettingsDoc) -> Result<(), String> {
        if doc.filtered() && !self.supported {
            return Err(
                "Audio exclusions require macOS 26 or newer. Your saved choice was not changed."
                    .into(),
            );
        }
        let encoded = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .put_setting(SETTINGS_KEY, &encoded)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;
    use fotw_web::capture::CaptureControl;
    #[test]
    fn encrypted_settings_control_refuses_unsupported_enable_but_allows_explicit_disable() {
        let control = SettingsControl {
            db: std::sync::Mutex::new(
                Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x42; 32])).unwrap(),
            ),
            supported: false,
        };
        let enabled = CaptureSettingsDoc {
            ignore_spotify: true,
            ignore_brave: false,
        };
        assert!(control.set(enabled).is_err());
        assert_eq!(
            control.read().settings.unwrap(),
            CaptureSettingsDoc::default()
        );
        control
            .db
            .lock()
            .unwrap()
            .put_setting(SETTINGS_KEY, &serde_json::to_string(&enabled).unwrap())
            .unwrap();
        control.set(CaptureSettingsDoc::default()).unwrap();
        assert_eq!(
            control.read().settings.unwrap(),
            CaptureSettingsDoc::default()
        );
    }
}

/// Read saved policy for diagnostics without creating a first-run library.
/// Only the absent default library uses the legacy all-system default. An
/// explicit library must already exist. An existing library
/// that cannot be read never silently becomes an unfiltered diagnostic recording.
///
/// # Errors
/// Inaccessible library path, key/library failure, or unenforceable saved policy.
pub fn configured_diagnostic(
    root: &std::path::Path,
    require_initialized: bool,
) -> Result<fotw_audio::SystemScope, String> {
    existing_policy(root, require_initialized, configured)
}

fn existing_policy(
    root: &std::path::Path,
    require_initialized: bool,
    load: impl FnOnce(&std::path::Path) -> Result<fotw_audio::SystemScope, String>,
) -> Result<fotw_audio::SystemScope, String> {
    let db_path = root.parent().unwrap_or(root).join("db.sqlite3");
    match std::fs::symlink_metadata(db_path) {
        Ok(_) => load(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !require_initialized => {
            Ok(fotw_audio::SystemScope::DefaultOutputMix)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("The selected library does not exist. Choose the directory containing db.sqlite3; no recording was started.".into())
        }
        Err(error) => Err(format!("Cannot inspect saved audio exclusions: {error}")),
    }
}
#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn standalone_diagnostics_do_not_create_a_library_or_read_keys() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            existing_policy(&dir.path().join("sessions"), false, |_| panic!(
                "no first-run key access"
            ))
            .unwrap(),
            fotw_audio::SystemScope::DefaultOutputMix
        );
        assert!(!dir.path().join("db.sqlite3").exists());
    }
    #[test]
    fn existing_unreadable_settings_cannot_become_an_unfiltered_mix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("db.sqlite3"),
            b"synthetic metadata marker; not a real database",
        )
        .unwrap();
        assert!(
            existing_policy(&dir.path().join("sessions"), false, |_| Err(
                "cannot unlock existing policy".into()
            ))
            .is_err()
        );
        let path = dir.path().join("not-a-directory");
        std::fs::write(&path, b"fixture").unwrap();
        assert!(
            existing_policy(&path.join("sessions"), false, |_| panic!(
                "bad path must refuse before loading"
            ))
            .is_err()
        );
    }
}

#[cfg(test)]
mod explicit_library_tests {
    use super::*;
    #[test]
    fn a_misspelled_explicit_library_never_silently_records_the_default_mix() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            existing_policy(&dir.path().join("sessions"), true, |_| panic!(
                "missing selected profile must not initialize a key"
            ))
            .is_err()
        );
        assert!(!dir.path().join("db.sqlite3").exists());
    }
}

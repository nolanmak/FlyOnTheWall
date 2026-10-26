//! Public installed-bundle metadata only; no app launch, process or audio access.
use crate::{AppRef, TapError};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSBundle, NSString};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub fn supported() -> bool {
    cidre::api::macos_available("26.0")
}

pub fn family(bundle_id: &str) -> Result<Vec<AppRef>, TapError> {
    if !supported() {
        return Err(TapError::unsupported(
            "application exclusions require macOS 26 or newer",
        ));
    }
    let app_name = match bundle_id {
        "com.spotify.client" => "Spotify",
        "com.brave.Browser" => "Brave",
        _ => "the selected app",
    };
    let url = NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))
        .ok_or_else(|| {
            TapError::unsupported(format!(
                "Install {app_name} before enabling its audio exclusion"
            ))
        })?;
    let path = url
        .path()
        .ok_or_else(|| TapError::unsupported("application has no local bundle path"))?;
    family_at(&PathBuf::from(path.to_string()), bundle_id)
}

fn identity(path: &Path) -> Option<String> {
    NSBundle::bundleWithPath(&NSString::from_str(path.to_str()?))?
        .bundleIdentifier()
        .map(|id| id.to_string())
}

fn family_at(root: &Path, expected: &str) -> Result<Vec<AppRef>, TapError> {
    if identity(root).as_deref() != Some(expected) {
        return Err(TapError::unsupported(
            "installed application identity does not match the selected exclusion",
        ));
    }
    let mut ids = BTreeSet::from([expected.to_owned()]);
    let mut pending = vec![root.join("Contents")];
    let mut visited = 0;
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| {
            TapError::platform(format!("could not inspect application helpers: {e}"))
        })? {
            let entry = entry.map_err(|e| {
                TapError::platform(format!("could not inspect application helper: {e}"))
            })?;
            visited += 1;
            if visited > 100_000 {
                return Err(TapError::unsupported(
                    "application helper metadata exceeds inspection limit",
                ));
            }
            let kind = entry
                .file_type()
                .map_err(|e| TapError::platform(e.to_string()))?;
            // Do not follow directory symlinks outside the installed bundle.
            if kind.is_symlink()
                && matches!(
                    entry.path().extension().and_then(|s| s.to_str()),
                    Some("app" | "xpc" | "appex")
                )
            {
                return Err(TapError::unsupported(
                    "application helper is a symlink; exclusion identity cannot be confirmed",
                ));
            }
            if !kind.is_dir() {
                continue;
            }
            let path = entry.path();
            if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("app" | "xpc" | "appex")
            ) {
                let id = identity(&path).ok_or_else(|| {
                    TapError::unsupported("an application helper has no readable bundle identity")
                })?;
                // Never broaden Brave/Spotify policy to Chrome or shared Apple helpers.
                if id != expected && !id.starts_with(&format!("{expected}.")) {
                    return Err(TapError::unsupported(
                        "application contains a helper outside its bundle identity family; exclusion cannot be confirmed",
                    ));
                }
                ids.insert(id);
            }
            pending.push(path);
        }
    }
    Ok(ids.into_iter().map(AppRef::BundleId).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct OwnedBundle(PathBuf);
    impl OwnedBundle {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "fotw-exclusion-bundle-{}-{unique}.app",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn put(&self, sub: &str, id: &str) {
            let dir = self.0.join(sub).join("Contents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Info.plist"), format!(r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>"#)).unwrap();
        }
    }
    impl Drop for OwnedBundle {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn installed_helper_metadata_is_resolved_even_without_any_running_process() {
        let bundle = OwnedBundle::new();
        bundle.put("", "com.brave.Browser");
        bundle.put(
            "Contents/Frameworks/Browser.framework/Versions/1/Helpers/Renderer.app",
            "com.brave.Browser.helper.renderer",
        );
        bundle.put("Contents/Helpers/Audio.app", "com.brave.Browser.helper");
        let ids = family_at(&bundle.0, "com.brave.Browser").unwrap();
        assert_eq!(
            ids,
            vec![
                AppRef::BundleId("com.brave.Browser".into()),
                AppRef::BundleId("com.brave.Browser.helper".into()),
                AppRef::BundleId("com.brave.Browser.helper.renderer".into())
            ]
        );
    }
    #[test]
    fn unconfirmed_or_unrelated_helper_identity_refuses_the_entire_exclusion() {
        let bundle = OwnedBundle::new();
        bundle.put("", "com.brave.Browser");
        bundle.put("Contents/Helpers/Other.app", "com.google.Chrome.helper");
        assert!(family_at(&bundle.0, "com.brave.Browser").is_err());
        assert!(family_at(&bundle.0, "com.spotify.client").is_err());
    }
}

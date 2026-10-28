//! `fotw-speechd-client`: the Rust side of the `fotw-speechd` helper (#243).
//!
//! FluidAudio's models (Parakeet, Silero VAD, and later Nemotron and the
//! diarizer) are Swift and Core ML. FlyOnTheWall and Murmur stay pure Rust by
//! running them in one signed helper process and speaking the protocol in
//! `docs/SPEECHD_PROTOCOL.md` to it over its stdin and stdout.
//!
//! - [`frame`] and [`message`] are the wire format, shared with the fake.
//! - [`SpeechdClient`] spawns and owns the helper, restarts it under a
//!   [`RestartPolicy`], and turns a crash into a prompt stream error.
//! - `fotw-speechd-fake`, a binary target of this crate, is a model-free helper
//!   for tests, here and in consumers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
pub mod frame;
pub mod message;
pub mod restart;

pub use client::{
    ClientError, Health, Loaded, Progress, SpeechStream, SpeechdClient, SpeechdConfig, Status,
    StreamConfig, StreamEvent, StreamFailure,
};
pub use restart::RestartPolicy;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The helper's file name in an app bundle's `Contents/Helpers`.
pub const HELPER_NAME: &str = "fotw-speechd";

/// Where the helper is: `FOTW_SPEECHD_PATH` if set, else
/// `Contents/Helpers/fotw-speechd` of the app bundle this executable runs
/// from. `None` when neither exists, for example in a non-macOS build.
#[must_use]
pub fn locate_helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    locate_helper_from(&exe, std::env::var_os("FOTW_SPEECHD_PATH"))
}

/// [`locate_helper`] with its inputs passed in.
#[must_use]
pub fn locate_helper_from(exe: &Path, override_path: Option<OsString>) -> Option<PathBuf> {
    if let Some(path) = override_path.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    // <App>.app/Contents/MacOS/<exe> -> <App>.app/Contents/Helpers/fotw-speechd
    let contents = exe.parent()?.parent()?;
    let helper = contents.join("Helpers").join(HELPER_NAME);
    helper.is_file().then_some(helper)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_is_found_beside_the_app_binary_or_by_override() {
        let root = std::env::temp_dir().join(format!("fotw-speechd-locate-{}", std::process::id()));
        let macos = root.join("FlyOnTheWall.app/Contents/MacOS");
        let helpers = root.join("FlyOnTheWall.app/Contents/Helpers");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(&helpers).unwrap();
        let exe = macos.join("fotwd");
        assert_eq!(
            locate_helper_from(&exe, None),
            None,
            "no helper bundled yet"
        );
        std::fs::write(helpers.join(HELPER_NAME), b"").unwrap();
        assert_eq!(
            locate_helper_from(&exe, None),
            Some(helpers.join(HELPER_NAME))
        );
        assert_eq!(
            locate_helper_from(&exe, Some("/opt/speechd".into())),
            Some(PathBuf::from("/opt/speechd"))
        );
        assert_eq!(
            locate_helper_from(&exe, Some(OsString::new())),
            Some(helpers.join(HELPER_NAME))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

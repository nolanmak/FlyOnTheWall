//! Test support for crates that drive `fotw-speechd-fake` (feature `testing`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// `target/<profile>/fotw-speechd-fake`, up to date with its source.
///
/// Cargo builds a dependency's binaries only for that crate's own tests, so a
/// test elsewhere finds whatever fake an earlier build left behind, which can
/// predate the protocol it is testing. This asks cargo to build it once per
/// test process, with the test binary's own profile; cargo makes that a
/// no-op when the fake is already fresh.
///
/// # Panics
/// If cargo cannot build the fake, or the test binary is not where cargo puts
/// test binaries (`target/<profile>/deps/`).
pub fn fake_helper() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().expect("the test binary's path");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("a test binary under target/<profile>/deps/");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut build = Command::new(cargo);
        build.args([
            "build",
            "--quiet",
            "-p",
            "fotw-speechd-client",
            "--bin",
            "fotw-speechd-fake",
        ]);
        match profile_dir.file_name().and_then(|name| name.to_str()) {
            Some("debug") | None => {}
            Some("release") => {
                build.arg("--release");
            }
            Some(profile) => {
                build.args(["--profile", profile]);
            }
        }
        let status = build.status().expect("run cargo");
        assert!(status.success(), "could not build fotw-speechd-fake");
        let path = profile_dir.join("fotw-speechd-fake");
        assert!(path.is_file(), "no fake helper at {}", path.display());
        path
    })
    .clone()
}

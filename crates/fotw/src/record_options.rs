//! Diagnostic recording shares the selected app library's immutable policy.
use fotw_audio::SystemScope;
use std::path::{Path, PathBuf};
pub(crate) struct RecordOptions {
    pub seconds: u64,
    output: PathBuf,
    library_sessions: PathBuf,
    require_initialized: bool,
}
impl RecordOptions {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut positional = Vec::new();
        let mut library = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--library" {
                if library.is_some() {
                    return Err("Choose --library only once.".into());
                }
                let path = args
                    .next()
                    .filter(|path| !path.is_empty() && !path.starts_with("--"))
                    .ok_or("--library requires the directory containing db.sqlite3.")?;
                library = Some(PathBuf::from(path).join("sessions"));
            } else if arg.starts_with("--") {
                return Err(format!("Unknown recording option: {arg}"));
            } else {
                positional.push(arg);
            }
        }
        if positional.len() > 2 {
            return Err("Use record [seconds] [output-directory] [--library directory].".into());
        }
        let seconds = positional
            .first()
            .map(|s| s.parse::<u64>())
            .transpose()
            .map_err(|_| "Recording seconds must be a nonnegative integer.")?
            .unwrap_or(10);
        Ok(Self {
            seconds,
            output: positional
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("fotw-sessions")),
            require_initialized: library.is_some(),
            library_sessions: library.unwrap_or_else(fotwd::default_session_root),
        })
    }

    pub fn run(
        &self,
        load: impl FnOnce(&Path, bool) -> Result<SystemScope, String>,
        capture: impl FnOnce(PathBuf, u64, SystemScope) -> Result<PathBuf, String>,
    ) -> Result<PathBuf, String> {
        let scope = load(&self.library_sessions, self.require_initialized)?;
        capture(self.output.clone(), self.seconds, scope)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn explicit_library_is_resolved_before_recording_and_reused_by_capture() {
        let options = RecordOptions::parse(&args(&[
            "5",
            "/fixture/output",
            "--library",
            "/fixture/selected-library",
        ]))
        .unwrap();
        let selected = SystemScope::AllExcept(vec![fotw_audio::AppRef::BundleId(
            "com.spotify.client".into(),
        )]);
        let result = options
            .run(
                |root, require_initialized| {
                    assert!(require_initialized);
                    assert_eq!(root, Path::new("/fixture/selected-library/sessions"));
                    Ok(selected.clone())
                },
                |output, seconds, scope| {
                    assert_eq!(seconds, 5);
                    assert_eq!(scope, selected);
                    Ok(output)
                },
            )
            .unwrap();
        assert_eq!(result, Path::new("/fixture/output"));
    }
    #[test]
    fn unreadable_or_unsupported_policy_cannot_start_unfiltered_diagnostic_capture() {
        let options = RecordOptions::parse(&args(&["5", "/fixture/output"])).unwrap();
        assert!(
            options
                .run(
                    |_, _| Err("cannot enforce exclusions".into()),
                    |_, _, _| panic!("capture must not start")
                )
                .is_err()
        );
    }
    #[test]
    fn malformed_library_options_do_not_choose_a_different_profile_silently() {
        for input in [
            vec!["--library"],
            vec!["--library", ""],
            vec!["--library", "/one", "--library", "/two"],
            vec!["--unknown"],
        ] {
            assert!(RecordOptions::parse(&args(&input)).is_err(), "{input:?}");
        }
    }
}

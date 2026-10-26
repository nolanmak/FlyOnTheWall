//! Native app doorway. The daemon initializes on workers, AppKit owns main.
use crate::serve::Launch;
use std::path::PathBuf;

/// Native launch is mandatory for the app's serving doorway. Other CLI
/// commands preserve their existing behavior.
#[must_use]
pub fn should_launch(bundle: bool, stdin_is_terminal: bool, args: &[String]) -> bool {
    cfg!(target_os = "macos")
        && (args.first().is_some_and(|a| a == "shell")
            || (bundle
                && (args.first().is_some_and(|a| a == "serve")
                    || (args.is_empty() && !stdin_is_terminal))))
}
/// Run a native shell and daemon with bounded runtime teardown after capture and
/// persistence quiesce. Does not start recording automatically.
///
/// # Errors
/// Runtime/startup/AppKit initialization failure or an unsupported platform.
#[cfg(target_os = "macos")]
pub fn run(root: PathBuf, launch: Launch, port: u16) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let stop = crate::session::StopSignal::new();
    let startup = crate::serve::NativeStartup {
        sender: Some(sender),
        gate: crate::native_bridge::RenderGate::new(std::time::Duration::from_secs(5)),
        stop: stop.clone(),
        recorder: None,
    };
    runtime.spawn(crate::serve::serve_native(root, launch, port, startup));
    let outcome =
        fotw_shell::platform::macos::run_bootstrapped(receiver, &fotw_shell::HotkeyMap::defaults());
    stop.stop();
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    outcome
}
/// Native app hosting is unavailable on this platform.
///
/// # Errors
/// Always returns an unsupported-platform error.
#[cfg(not(target_os = "macos"))]
pub fn run(_root: PathBuf, _launch: Launch, _port: u16) -> Result<(), String> {
    Err("the native shell requires macOS".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_app_serving_launches_select_the_native_doorway() {
        let args = |values: &[&str]| values.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(should_launch(true, false, &[]), cfg!(target_os = "macos"));
        assert_eq!(
            should_launch(true, true, &args(&["serve"])),
            cfg!(target_os = "macos")
        );
        for command in ["record", "key", "stt", "engine", "doctor"] {
            assert!(!should_launch(true, false, &args(&[command])));
        }
        assert!(!should_launch(false, false, &args(&["serve"])));
        assert_eq!(
            should_launch(true, false, &args(&["serve", "--headless"])),
            cfg!(target_os = "macos")
        );
        assert!(!should_launch(true, true, &[]));
        assert_eq!(
            should_launch(false, true, &args(&["shell"])),
            cfg!(target_os = "macos")
        );
        assert_eq!(
            should_launch(true, false, &args(&["shell"])),
            cfg!(target_os = "macos")
        );
    }
}

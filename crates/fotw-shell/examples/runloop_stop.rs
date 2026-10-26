//! Owned no-input AppKit timer-stop fixture, never an installed-product launch.
#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    let elapsed = fotw_shell::platform::macos::probe_runloop_stop()?;
    assert!(elapsed < std::time::Duration::from_secs(2));
    println!("owned empty AppKit loop stopped without input in {elapsed:?}");
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("AppKit run-loop fixture requires macOS");
}

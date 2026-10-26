//! Owned view-only rendering: no daemon, capture, tray, keys or global hotkeys.
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use fotw_shell::{Level, PillView, Tone};
    use std::time::Duration;
    let output = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: indicator_frames <owned-output-directory>")?,
    );
    std::fs::create_dir_all(&output)?;
    for (name, status, elapsed, detail, tone, stop) in [
        (
            "preparing",
            "Preparing",
            "—",
            "Preparing local transcription; capture has not started",
            Tone::Preparing,
            true,
        ),
        (
            "recording",
            "Recording",
            "1:02:03",
            "System: whisper.cpp (small.en) · Mic: whisper.cpp (small.en)",
            Tone::Live,
            true,
        ),
        (
            "finishing",
            "Finishing",
            "1:02:03",
            "Capture stopped; waiting for transcription and audio finalization",
            Tone::Finishing,
            false,
        ),
        (
            "recovery",
            "Save needs recovery",
            "—",
            "A library save was not confirmed. Check recovery audio on next launch.",
            Tone::Fault,
            false,
        ),
    ] {
        let view = PillView {
            elapsed: Duration::from_secs(3723),
            elapsed_label: elapsed.into(),
            status_label: status,
            level: Level::SILENT,
            level_available: false,
            detail: detail.into(),
            tone,
            stop_enabled: stop,
        };
        let probe = fotw_shell::platform::macos::probe_indicator_frame(
            &view,
            &output.join(format!("{name}.png")),
        )?;
        assert!(probe.nonactivating && probe.on_screen && probe.controls_inside_panel);
        assert_eq!(probe.status, status);
        assert_eq!(probe.elapsed, elapsed);
        assert_eq!(probe.detail, detail);
        assert!(probe.meter.is_empty());
        assert_eq!(probe.stop_enabled, stop);
        assert_eq!(probe.stop_dispatched, stop);
        std::fs::write(output.join(format!("{name}.txt")), format!("{probe:#?}\n"))?;
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("owned native indicator frames require macOS");
}

//! Owned AppKit consent + actual NativeHost/bridge, with capture forbidden.
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use fotw_shell::{HotkeyAction, HotkeyMap, MenuAction, Monotonic, SurfaceEvent, SurfaceHost};
    use fotw_web::RecorderControl;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};
    let output = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: native_consent_fixture <owned-output-directory>")?,
    );
    std::fs::create_dir_all(&output)?;
    let lifecycle = fotw_shell::platform::macos::probe_application_events()?;
    assert!(
        lifecycle.reopen_queued_once
            && lifecycle.quit_queued_once
            && lifecycle.immediate_termination_cancelled
    );
    std::fs::write(output.join("lifecycle.txt"), format!("{lifecycle:#?}\n"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    for (name, event) in [
        ("menu", SurfaceEvent::Menu(MenuAction::ToggleRecording)),
        (
            "hotkey",
            SurfaceEvent::Chord(
                HotkeyMap::defaults()
                    .chord_for(HotkeyAction::ToggleRecording)
                    .unwrap(),
            ),
        ),
    ] {
        let root = tempfile::Builder::new()
            .prefix("owned-consent-")
            .tempdir_in(&output)?;
        let gate = fotwd::native_bridge::RenderGate::new(Duration::from_secs(5));
        let configured = Arc::new(AtomicU64::new(0));
        let configured_worker = configured.clone();
        let rec = Arc::new(
            fotwd::recording::DaemonRecorder::with_parts(
                root.path().join("sessions"),
                runtime.handle().clone(),
                fotwd::session::SegmentTap::default(),
                fotwd::recording::ReadyTap::default(),
                Box::new(|| panic!("owned consent fixture must never open capture")),
                Box::new(move || {
                    configured_worker.fetch_add(1, Ordering::SeqCst);
                    fotwd::session::Transcription::Disabled
                }),
                Box::new(|_, _| None),
                Duration::from_secs(1),
                Duration::from_secs(5),
            )
            .with_start_visibility(gate.admission()),
        );
        let bridge = fotwd::native_bridge::RecorderBridge::spawn(rec.clone(), gate)?;
        let (actions, _receiver) = mpsc::sync_channel(4);
        let mut host = fotwd::native_host::NativeHost::new(
            bridge,
            HotkeyMap::defaults(),
            "Everyone must agree before this meeting is recorded.".into(),
            actions,
        );
        let began = Instant::now();
        while !host.view().menu.record.enabled {
            assert!(began.elapsed() < Duration::from_secs(5));
            host.tick(Monotonic::ZERO);
            std::thread::sleep(Duration::from_millis(10));
        }
        host.on_event(event, Monotonic::ZERO);
        let probe =
            fotw_shell::platform::macos::probe_consent_controls(&mut host, Monotonic::ZERO)?;
        assert!(
            probe.nonactivating
                && probe.on_screen
                && probe.unchecked_start_disabled
                && probe.unchecked_click_blocked
                && probe.acknowledgement_delivered
                && probe.acknowledged_start_enabled
                && probe.start_response_acknowledged
                && probe.prompt_cleared
        );
        let began = Instant::now();
        while !rec.status().is_active() {
            assert!(began.elapsed() < Duration::from_secs(5));
            host.tick(Monotonic::ZERO);
            std::thread::sleep(Duration::from_millis(10));
        }
        // No indicator frame was acknowledged: even configuration must wait.
        assert_eq!(configured.load(Ordering::SeqCst), 0);
        host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
        let began = Instant::now();
        while !host.should_exit() {
            assert!(began.elapsed() < Duration::from_secs(5));
            host.tick(Monotonic::ZERO);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(configured.load(Ordering::SeqCst), 0);
        assert!(rec.shutdown_status().drained());
        assert!(!rec.shutdown_status().save_failed);
        let audit = std::fs::read_to_string(root.path().join("audit.jsonl"))?;
        assert!(audit.contains(&format!("\"origin\":\"{name}\"")));
        std::fs::write(
            output.join(format!("{name}.txt")),
            format!(
                "{probe:#?}\nconfiguration_calls=0\ncapture_calls=0\nshutdown_drained=true\naudit_origin={name}\n"
            ),
        )?;
    }
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("owned native consent fixture requires macOS");
}

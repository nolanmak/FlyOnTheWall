//! Synthetic, device-free native render and command admission checks.
use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{SampleFormat, StreamFormat, TapId};
use fotw_web::{RecorderControl, RecordingState};
use fotwd::native_bridge::RenderGate;
use fotwd::recording::{DaemonRecorder, ReadyTap};
use fotwd::session::{SegmentTap, StopSignal, Transcription};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition did not become true");
}
fn recorder(
    root: &std::path::Path,
    gate: &RenderGate,
    opened: Arc<AtomicU64>,
) -> Arc<DaemonRecorder> {
    recorder_with_factory(root, gate, opened, Box::new(|| Transcription::Disabled))
}
fn recorder_with_factory(
    root: &std::path::Path,
    gate: &RenderGate,
    opened: Arc<AtomicU64>,
    factory: fotwd::recording::TranscriptionFactory,
) -> Arc<DaemonRecorder> {
    recorder_with_finisher(root, gate, opened, factory, Box::new(|_, _| None))
}
fn recorder_with_finisher(
    root: &std::path::Path,
    gate: &RenderGate,
    opened: Arc<AtomicU64>,
    factory: fotwd::recording::TranscriptionFactory,
    finish: fotwd::recording::Finisher,
) -> Arc<DaemonRecorder> {
    Arc::new(
        DaemonRecorder::with_parts(
            root.join("sessions"),
            tokio::runtime::Handle::current(),
            SegmentTap::default(),
            ReadyTap::default(),
            Box::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Ok((
                    Box::new(FileAudioSource::from_wav(
                        TapId::system_default(),
                        WavData {
                            format: StreamFormat::new(16_000, 1, SampleFormat::I16),
                            samples: vec![0.0; 160_000],
                        },
                        ReplaySpeed::Realtime,
                    )),
                    None,
                ))
            }),
            factory,
            finish,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .with_start_visibility(gate.admission()),
    )
}
#[tokio::test(flavor = "multi_thread")]
async fn capture_waits_for_its_actual_preparing_frame() {
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let worker = rec.clone();
    let start = tokio::task::spawn_blocking(move || worker.start());
    until(|| rec.status().is_active()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        opened.load(Ordering::SeqCst),
        0,
        "capture opened before a rendered frame"
    );
    let observed = rec.observe();
    assert_eq!(observed.status().state, RecordingState::Preparing);
    until(|| gate.did_render(observed.stop_target().unwrap())).await;
    assert_eq!(
        start.await.unwrap().unwrap().state,
        RecordingState::Recording
    );
    rec.stop().unwrap();
    until(|| rec.status().state == RecordingState::Idle).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn missing_renderer_refuses_capture_and_releases_the_attempt() {
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_millis(100));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let worker = rec.clone();
    let start = tokio::task::spawn_blocking(move || worker.start())
        .await
        .unwrap();
    if start.is_ok() {
        rec.stop().unwrap();
    }
    assert!(start.is_err(), "missing renderer admitted capture");
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    assert_eq!(rec.status().state, RecordingState::Idle);
}

async fn snapshot_until(
    bridge: &mut fotwd::native_bridge::RecorderBridge,
    predicate: impl Fn(&fotwd::native_bridge::BridgeSnapshot) -> bool,
) -> fotwd::native_bridge::BridgeSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(snapshot) = bridge.poll()
                && predicate(&snapshot)
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bridge did not publish the expected state")
}
#[tokio::test(flavor = "multi_thread")]
async fn bridge_stays_nonblocking_and_cancels_before_render() {
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let mut bridge = fotwd::native_bridge::RecorderBridge::spawn(rec.clone(), gate).unwrap();
    assert!(
        bridge
            .start(fotw_shell::StartOrigin::Hotkey, false)
            .is_err()
    );
    assert!(!root.path().join("audit.jsonl").exists());
    let ticket = bridge.start(fotw_shell::StartOrigin::Hotkey, true).unwrap();
    assert!(bridge.start(fotw_shell::StartOrigin::Menu, true).is_err());
    let preparing = snapshot_until(&mut bridge, |s| {
        s.observation.status().state == RecordingState::Preparing
    })
    .await;
    assert!(preparing.observation.status().session_id.is_none());
    ticket.cancel();
    let failed = snapshot_until(&mut bridge, |s| {
        s.start_outcome
            .as_ref()
            .is_some_and(|o| o.id == ticket.id())
    })
    .await;
    assert!(failed.start_outcome.unwrap().error.is_some());
    assert_eq!(failed.observation.status().state, RecordingState::Idle);
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}
#[tokio::test(flavor = "multi_thread")]
async fn stale_frames_stop_commands_and_tickets_cannot_reach_a_new_attempt() {
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let mut bridge = fotwd::native_bridge::RecorderBridge::spawn(rec.clone(), gate).unwrap();
    let old_ticket = bridge.start(fotw_shell::StartOrigin::Menu, true).unwrap();
    let old = snapshot_until(&mut bridge, |s| {
        s.observation.status().state == RecordingState::Preparing
    })
    .await;
    let old_target = old.observation.stop_target().unwrap().clone();
    old_ticket.cancel();
    snapshot_until(&mut bridge, |s| {
        s.start_outcome
            .as_ref()
            .is_some_and(|o| o.id == old_ticket.id())
    })
    .await;
    let ticket = bridge.start(fotw_shell::StartOrigin::Menu, true).unwrap();
    let next = snapshot_until(&mut bridge, |s| {
        s.observation.status().state == RecordingState::Preparing
    })
    .await;
    old_ticket.cancel();
    assert!(!bridge.did_render(&old_target));
    bridge.stop(&old_target).unwrap();
    let rejected = snapshot_until(&mut bridge, |s| s.command_error.is_some()).await;
    assert_eq!(
        rejected.observation.status().state,
        RecordingState::Preparing
    );
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    until(|| bridge.did_render(next.observation.stop_target().unwrap())).await;
    let live = snapshot_until(&mut bridge, |s| {
        s.observation.status().state == RecordingState::Recording
            && s.start_outcome
                .as_ref()
                .is_some_and(|o| o.id == ticket.id())
    })
    .await;
    assert!(live.start_outcome.unwrap().error.is_none());
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    bridge
        .stop(live.observation.stop_target().unwrap())
        .unwrap();
    snapshot_until(&mut bridge, |s| {
        s.observation.status().state == RecordingState::Idle
    })
    .await;
}
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_surface_refuses_pending_capture_and_stops_live_capture() {
    for live_first in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let gate = RenderGate::new(Duration::from_secs(3));
        let opened = Arc::new(AtomicU64::new(0));
        let rec = recorder(root.path(), &gate, opened.clone());
        let mut bridge = fotwd::native_bridge::RecorderBridge::spawn(rec.clone(), gate).unwrap();
        bridge.start(fotw_shell::StartOrigin::Menu, true).unwrap();
        let preparing = snapshot_until(&mut bridge, |s| {
            s.observation.status().state == RecordingState::Preparing
        })
        .await;
        if live_first {
            until(|| bridge.did_render(preparing.observation.stop_target().unwrap())).await;
            snapshot_until(&mut bridge, |s| {
                s.observation.status().state == RecordingState::Recording
            })
            .await;
        }
        drop(bridge);
        until(|| rec.status().state == RecordingState::Idle).await;
        assert_eq!(opened.load(Ordering::SeqCst), u64::from(live_first));
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn closed_surface_and_wrong_recorder_frame_never_admit_capture() {
    let root = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let other_gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let other = recorder(other_root.path(), &other_gate, opened.clone());
    let cancel = StopSignal::new();
    let request_cancel = cancel.clone();
    let worker = other.clone();
    let other_start = tokio::task::spawn_blocking(move || {
        worker.start_native(fotw_shell::StartOrigin::Menu, true, request_cancel)
    });
    until(|| other.status().state == RecordingState::Preparing).await;
    let foreign = other.observe();
    let worker = rec.clone();
    let start = tokio::task::spawn_blocking(move || worker.start());
    until(|| rec.status().state == RecordingState::Preparing).await;
    assert!(!gate.did_render(foreign.stop_target().unwrap()));
    gate.close();
    cancel.stop();
    assert!(start.await.unwrap().is_err());
    assert!(other_start.await.unwrap().is_err());
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_configuration_does_not_block_bridge_status_or_stop() {
    use std::sync::{Condvar, Mutex};
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
    let config_release = release.0.clone();
    let entered = Arc::new(AtomicU64::new(0));
    let config_entered = entered.clone();
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder_with_factory(
        root.path(),
        &gate,
        opened.clone(),
        Box::new(move || {
            config_entered.fetch_add(1, Ordering::SeqCst);
            let (lock, changed) = &*config_release;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
            Transcription::Disabled
        }),
    );
    let mut bridge = fotwd::native_bridge::RecorderBridge::spawn(rec.clone(), gate).unwrap();
    let ticket = bridge.start(fotw_shell::StartOrigin::Menu, true).unwrap();
    let preparing = snapshot_until(&mut bridge, |s| {
        s.observation.status().state == RecordingState::Preparing
    })
    .await;
    until(|| bridge.did_render(preparing.observation.stop_target().unwrap())).await;
    until(|| entered.load(Ordering::SeqCst) == 1).await;
    let responsive = snapshot_until(&mut bridge, |s| s.sequence > preparing.sequence).await;
    bridge
        .stop(responsive.observation.stop_target().unwrap())
        .unwrap();
    let cancelled = snapshot_until(&mut bridge, |s| {
        s.observation.status().preparation_cancel_requested
    })
    .await;
    assert!(
        cancelled.start_outcome.is_none(),
        "configuration is still blocked"
    );
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    assert!(bridge.start(fotw_shell::StartOrigin::Menu, true).is_err());
    drop(release);
    let finished = snapshot_until(&mut bridge, |s| {
        s.start_outcome
            .as_ref()
            .is_some_and(|o| o.id == ticket.id())
    })
    .await;
    assert!(finished.start_outcome.unwrap().error.is_some());
    assert_eq!(finished.observation.status().state, RecordingState::Idle);
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

fn native_host(rec: Arc<DaemonRecorder>, gate: RenderGate) -> fotwd::native_host::NativeHost {
    let bridge = fotwd::native_bridge::RecorderBridge::spawn(rec, gate).unwrap();
    let (actions, _receiver) = std::sync::mpsc::sync_channel(4);
    fotwd::native_host::NativeHost::new(
        bridge,
        fotw_shell::HotkeyMap::defaults(),
        "Everyone must consent.".into(),
        actions,
    )
}
#[tokio::test(flavor = "multi_thread")]
async fn native_menu_and_hotkey_require_a_fresh_explicit_consent_prompt() {
    use fotw_shell::{
        HotkeyAction, HotkeyMap, MenuAction, Monotonic, PromptChoice, SurfaceEvent, SurfaceHost,
    };
    for event in [
        SurfaceEvent::Menu(MenuAction::ToggleRecording),
        SurfaceEvent::Chord(
            HotkeyMap::defaults()
                .chord_for(HotkeyAction::ToggleRecording)
                .unwrap(),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let gate = RenderGate::new(Duration::from_secs(3));
        let opened = Arc::new(AtomicU64::new(0));
        let rec = recorder(root.path(), &gate, opened.clone());
        let mut host = native_host(rec.clone(), gate);
        until(|| {
            host.tick(Monotonic::ZERO);
            host.view().menu.record.enabled
        })
        .await;
        host.on_event(event, Monotonic::ZERO);
        let prompt = host.view().prompt.expect("manual Start must show consent");
        assert!(prompt.requires_acknowledgement);
        assert!(!prompt.acknowledged);
        assert!(!prompt.start_enabled);
        host.on_event(
            SurfaceEvent::Prompt(PromptChoice::Start {
                acknowledged: false,
            }),
            Monotonic::ZERO,
        );
        assert_eq!(rec.status().state, RecordingState::Idle);
        assert_eq!(opened.load(Ordering::SeqCst), 0);
        host.on_event(SurfaceEvent::Acknowledge(true), Monotonic::ZERO);
        host.on_event(
            SurfaceEvent::Prompt(PromptChoice::Start { acknowledged: true }),
            Monotonic::ZERO,
        );
        until(|| {
            host.tick(Monotonic::from_secs(500));
            let view = host.view();
            if view.pill.is_some() {
                host.did_render();
            }
            rec.status().state == RecordingState::Recording
        })
        .await;
        host.on_event(SurfaceEvent::Stop, Monotonic::from_secs(500));
        until(|| {
            host.tick(Monotonic::from_secs(500));
            rec.status().state == RecordingState::Idle
        })
        .await;
        until(|| {
            host.tick(Monotonic::from_secs(500));
            host.view().menu.record.enabled && host.view().pill.is_none()
        })
        .await;
        host.on_event(
            SurfaceEvent::Menu(MenuAction::ToggleRecording),
            Monotonic::from_secs(500),
        );
        assert!(!host.view().prompt.unwrap().acknowledged);
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_start_is_visible_without_a_native_start_or_speculative_clock() {
    use fotw_shell::{Monotonic, SurfaceEvent, SurfaceHost};
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let mut host = native_host(rec.clone(), gate);
    let worker = rec.clone();
    let start = tokio::task::spawn_blocking(move || worker.start());
    until(|| rec.status().state == RecordingState::Preparing).await;
    until(|| {
        host.tick(Monotonic::from_secs(800));
        host.view().pill.is_some()
    })
    .await;
    let pill = host.view().pill.unwrap();
    assert_eq!(pill.status_label, "Preparing");
    assert_eq!(pill.elapsed_label, "—");
    assert!(!pill.level_available, "no measured RMS was supplied");
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    until(|| {
        host.did_render();
        host.tick(Monotonic::from_secs(800));
        rec.status().state == RecordingState::Recording
    })
    .await;
    assert!(start.await.unwrap().is_ok());
    host.on_event(SurfaceEvent::Stop, Monotonic::from_secs(800));
    until(|| {
        host.tick(Monotonic::from_secs(800));
        rec.status().state == RecordingState::Idle
    })
    .await;
    until(|| {
        host.tick(Monotonic::from_secs(800));
        host.view().pill.is_none()
    })
    .await;
    assert!(!host.view().menu.status.contains("Saved"));
    host.on_event(SurfaceEvent::Terminate, Monotonic::from_secs(1));
    assert!(
        !host.should_exit(),
        "OS termination must not acknowledge failed-save recovery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_ui_action_failure_does_not_label_a_new_dashboard_capture_as_failed() {
    use fotw_shell::{MenuAction, Monotonic, SurfaceEvent, SurfaceHost};
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened);
    let mut host = native_host(rec.clone(), gate);
    until(|| {
        host.tick(Monotonic::ZERO);
        host.view().menu.record.enabled
    })
    .await;
    // This fixture has no background menu receiver; delivery correctly fails.
    host.on_event(SurfaceEvent::Menu(MenuAction::About), Monotonic::ZERO);
    assert!(
        host.view()
            .pill
            .unwrap()
            .detail
            .contains("could not be queued")
    );
    let worker = rec.clone();
    let start = tokio::task::spawn_blocking(move || worker.start());
    until(|| {
        host.tick(Monotonic::ZERO);
        host.view().tray.state == fotw_shell::TrayState::Preparing
    })
    .await;
    assert!(
        !host
            .view()
            .pill
            .unwrap()
            .detail
            .contains("could not be queued"),
        "a new observed attempt must replace an old UI failure"
    );
    until(|| {
        host.did_render();
        host.tick(Monotonic::ZERO);
        rec.status().state == RecordingState::Recording
    })
    .await;
    assert!(start.await.unwrap().is_ok());
    host.on_event(SurfaceEvent::Stop, Monotonic::ZERO);
    until(|| rec.status().state == RecordingState::Idle).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn user_cancelled_native_preparation_returns_to_idle_without_a_failure_badge() {
    use fotw_shell::{MenuAction, Monotonic, PromptChoice, SurfaceEvent, SurfaceHost};
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let mut host = native_host(rec.clone(), gate);
    until(|| {
        host.tick(Monotonic::ZERO);
        host.view().menu.record.enabled
    })
    .await;
    host.on_event(
        SurfaceEvent::Menu(MenuAction::ToggleRecording),
        Monotonic::ZERO,
    );
    host.on_event(SurfaceEvent::Acknowledge(true), Monotonic::ZERO);
    host.on_event(
        SurfaceEvent::Prompt(PromptChoice::Start { acknowledged: true }),
        Monotonic::ZERO,
    );
    assert!(host.view().pill.is_some());
    host.on_event(SurfaceEvent::Stop, Monotonic::ZERO);
    until(|| {
        host.tick(Monotonic::ZERO);
        rec.status().state == RecordingState::Idle && host.view().menu.record.enabled
    })
    .await;
    assert!(
        host.view().pill.is_none(),
        "deliberate preparation cancellation is not a failure"
    );
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn native_quit_waits_for_persistence_and_requires_acknowledgement_of_save_failure() {
    use fotw_shell::{MenuAction, Monotonic, PromptChoice, SurfaceEvent, SurfaceHost};
    use std::sync::{Condvar, Mutex};
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
    let finish_release = release.0.clone();
    let entered = Arc::new(AtomicU64::new(0));
    let finish_entered = entered.clone();
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder_with_finisher(
        root.path(),
        &gate,
        opened,
        Box::new(|| Transcription::Disabled),
        Box::new(move |_, _| {
            finish_entered.fetch_add(1, Ordering::SeqCst);
            let (lock, changed) = &*finish_release;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
            None
        }),
    );
    let mut host = native_host(rec.clone(), gate);
    until(|| {
        host.tick(Monotonic::ZERO);
        host.view().menu.record.enabled
    })
    .await;
    host.on_event(
        SurfaceEvent::Menu(MenuAction::ToggleRecording),
        Monotonic::ZERO,
    );
    host.on_event(SurfaceEvent::Acknowledge(true), Monotonic::ZERO);
    host.on_event(
        SurfaceEvent::Prompt(PromptChoice::Start { acknowledged: true }),
        Monotonic::ZERO,
    );
    until(|| {
        host.tick(Monotonic::ZERO);
        host.did_render();
        rec.status().state == RecordingState::Recording
    })
    .await;
    host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
    assert!(
        rec.shutdown_status().closing,
        "Quit closes Start admission synchronously before releasing capture"
    );
    until(|| entered.load(Ordering::SeqCst) == 1).await;
    assert!(
        rec.shutdown_status().closing,
        "native Quit must close dashboard Start admission"
    );
    assert_eq!(rec.status().state, RecordingState::Idle);
    host.tick(Monotonic::ZERO);
    assert!(
        !host.should_exit(),
        "the persistence worker still owns audio"
    );
    assert!(rec.start().is_err());
    host.tick(Monotonic::from_secs(31));
    assert!(
        host.view()
            .pill
            .unwrap()
            .detail
            .contains("app remains open")
    );
    assert!(!host.should_exit());
    drop(release);
    until(|| {
        host.tick(Monotonic::from_secs(1));
        host.view().menu.quit.label.contains("recover")
    })
    .await;
    assert!(
        !host.should_exit(),
        "a missing save must be acknowledged before exit"
    );
    assert!(!host.view().menu.status.contains("Saved"));
    host.on_event(
        SurfaceEvent::Menu(MenuAction::Quit),
        Monotonic::from_secs(1),
    );
    assert!(host.should_exit());
}

#[tokio::test(flavor = "multi_thread")]
async fn native_quit_before_the_first_render_cancels_without_save_failure_or_capture() {
    use fotw_shell::{MenuAction, Monotonic, PromptChoice, SurfaceEvent, SurfaceHost};
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let mut host = native_host(rec.clone(), gate);
    until(|| {
        host.tick(Monotonic::ZERO);
        host.view().menu.record.enabled
    })
    .await;
    host.on_event(
        SurfaceEvent::Menu(MenuAction::ToggleRecording),
        Monotonic::ZERO,
    );
    host.on_event(SurfaceEvent::Acknowledge(true), Monotonic::ZERO);
    host.on_event(
        SurfaceEvent::Prompt(PromptChoice::Start { acknowledged: true }),
        Monotonic::ZERO,
    );
    host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
    until(|| {
        host.tick(Monotonic::ZERO);
        host.should_exit()
    })
    .await;
    assert!(rec.shutdown_status().drained());
    assert!(!rec.shutdown_status().save_failed);
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    assert!(!host.view().menu.record.enabled);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_daemon_disables_start_and_shows_failure_until_explicit_quit() {
    use fotw_shell::{MenuAction, Monotonic, SurfaceEvent, SurfaceHost};
    let root = tempfile::tempdir().unwrap();
    let gate = RenderGate::new(Duration::from_secs(3));
    let opened = Arc::new(AtomicU64::new(0));
    let rec = recorder(root.path(), &gate, opened.clone());
    let mut host = native_host(rec.clone(), gate);
    until(|| {
        host.tick(Monotonic::ZERO);
        host.view().menu.record.enabled
    })
    .await;
    rec.begin_shutdown();
    until(|| {
        host.tick(Monotonic::ZERO);
        !host.view().menu.record.enabled
    })
    .await;
    assert!(host.view().pill.unwrap().detail.contains("daemon stopped"));
    assert!(
        !host.should_exit(),
        "a service failure must remain visible until acknowledged"
    );
    host.on_event(
        SurfaceEvent::Menu(MenuAction::ToggleRecording),
        Monotonic::ZERO,
    );
    assert!(host.view().prompt.is_none());
    host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
    assert!(host.should_exit());
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

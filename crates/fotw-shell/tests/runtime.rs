//! The glue between the state machine and the host, driven headless.
//!
//! On a real Mac this path runs inside `NSApplication::run()` and is reached
//! by clicking a menu bar. Here it is a `FakeHost` and a call log, which is
//! the only way it gets exercised at all.

use fotw_shell::testing::{FakeHost, HostCall};
use fotw_shell::{
    Chord, DetectedMeeting, DetectionUpdate, HotkeyAction, HotkeyMap, Key, Level, MenuAction,
    Modifiers, Monotonic, PromptChoice, ShellError, ShellRuntime, StartOrigin,
};

fn runtime() -> (ShellRuntime<FakeHost>, FakeHost) {
    let host = FakeHost::new();
    (ShellRuntime::new(host.clone()), host)
}

fn toggle_chord() -> Chord {
    HotkeyMap::defaults()
        .chord_for(HotkeyAction::ToggleRecording)
        .expect("the toggle has a default")
}

#[test]
fn the_toggle_hotkey_starts_then_stops_capture() {
    let (mut rt, host) = runtime();

    assert!(rt.on_chord(toggle_chord(), Monotonic::ZERO));
    assert_eq!(host.count(&HostCall::StartCapture), 1);
    assert!(rt.core().capture_is_live());

    assert!(rt.on_chord(toggle_chord(), Monotonic::from_secs(30)));
    assert_eq!(host.count(&HostCall::StopCapture), 1);
    assert!(!rt.core().capture_is_live());
    assert!(
        rt.view().pill.is_some(),
        "the session is still being written; the indicator stays"
    );
}

#[test]
fn an_unbound_chord_is_reported_and_ignored() {
    let (mut rt, host) = runtime();
    let unbound = Chord::new(Modifiers::CONTROL, Key::Letter('j'));

    assert!(!rt.on_chord(unbound, Monotonic::ZERO));
    assert!(host.calls().is_empty());
}

#[test]
fn a_failed_start_faults_and_tears_the_capture_down() {
    let (mut rt, host) = runtime();
    host.fail_start("no system audio permission");

    rt.on_menu(MenuAction::ToggleRecording, Monotonic::ZERO);

    assert_eq!(
        host.calls(),
        vec![
            // The consent record is written for the *request*, not for the
            // successful capture: the user asked to record, and CON-08's log
            // is a record of what was asked for as well as what happened.
            HostCall::AuditStart(StartOrigin::Menu),
            HostCall::StartCaptureFailed,
            HostCall::SetTicking(true),
            HostCall::StopCapture,
        ],
        "a failed start must still tear down whatever was half-built"
    );
    assert!(!rt.core().capture_is_live());

    let pill = rt
        .view()
        .pill
        .expect("a failure must be reported, not swallowed");
    assert_eq!(pill.status_label, "Recording failed");
    assert!(
        rt.view()
            .tray
            .tooltip
            .contains("no system audio permission"),
        "the host's reason must reach the user: {}",
        rt.view().tray.tooltip
    );
}

#[test]
fn a_failed_start_does_not_loop_forever() {
    // The fault feeds `CaptureFailed` back into the core, which emits
    // `StopCapture`. If that ever emitted `StartCapture` again this would
    // spin; the dispatcher bounds it, and this test would hang without it.
    let (mut rt, host) = runtime();
    host.fail_start("nope");
    for step in 0..50u64 {
        rt.on_menu(MenuAction::ToggleRecording, Monotonic::from_secs(step));
        rt.dismiss();
    }
    assert_eq!(host.count(&HostCall::StartCaptureFailed), 50);
}

#[test]
fn ticking_is_requested_on_leaving_idle_and_dropped_on_return() {
    let (mut rt, host) = runtime();

    rt.on_chord(toggle_chord(), Monotonic::ZERO);
    assert_eq!(host.count(&HostCall::SetTicking(true)), 1);
    assert_eq!(host.count(&HostCall::SetTicking(false)), 0);

    rt.request_stop();
    rt.capture_finished(Monotonic::from_secs(1));
    assert_eq!(
        host.count(&HostCall::SetTicking(false)),
        0,
        "the linger timer still needs ticks"
    );

    rt.dismiss();
    assert_eq!(host.count(&HostCall::SetTicking(false)), 1);

    // And no spurious repeats once idle.
    rt.tick(Monotonic::from_secs(2));
    rt.tick(Monotonic::from_secs(3));
    assert_eq!(host.count(&HostCall::SetTicking(false)), 1);
}

#[test]
fn the_level_is_polled_only_while_recording() {
    let (mut rt, host) = runtime();
    host.set_level(Level::new(0.8));

    // Idle: no session, so no meter to feed.
    rt.tick(Monotonic::from_secs(1));
    assert!(rt.view().pill.is_none());

    rt.on_chord(toggle_chord(), Monotonic::from_secs(2));
    rt.tick(Monotonic::from_secs(3));
    assert_eq!(rt.view().pill.unwrap().level, Level::new(0.8));

    // After the stop, the meter must not keep reading the host.
    rt.request_stop();
    host.set_level(Level::new(1.0));
    rt.tick(Monotonic::from_secs(4));
    assert_eq!(rt.view().pill.unwrap().level, Level::SILENT);
}

#[test]
fn elapsed_advances_through_the_runtime_tick() {
    let (mut rt, _host) = runtime();
    rt.on_chord(toggle_chord(), Monotonic::ZERO);
    rt.tick(Monotonic::from_secs(754));
    assert_eq!(rt.view().pill.unwrap().elapsed_label, "12:34");
    assert_eq!(rt.view().tray.title.as_deref(), Some("12:34"));
}

#[test]
fn every_menu_row_reaches_the_host() {
    let cases = [
        (MenuAction::OpenNotes, HostCall::OpenNotes),
        (MenuAction::DisclosureKit, HostCall::OpenDisclosureKit),
        (MenuAction::Settings, HostCall::OpenSettings),
        (MenuAction::About, HostCall::OpenAbout),
        (MenuAction::Quit, HostCall::Quit),
    ];
    for (action, expected) in cases {
        let (mut rt, host) = runtime();
        rt.on_menu(action, Monotonic::ZERO);
        assert_eq!(host.calls(), vec![expected], "{action:?}");
    }
}

#[test]
fn the_stop_button_and_the_menu_agree() {
    // The pill's Stop button and the menu's Stop row are two entry points to
    // one transition. They must not be able to disagree.
    let (mut rt_button, host_button) = runtime();
    rt_button.on_chord(toggle_chord(), Monotonic::ZERO);
    rt_button.request_stop();

    let (mut rt_menu, host_menu) = runtime();
    rt_menu.on_chord(toggle_chord(), Monotonic::ZERO);
    rt_menu.on_menu(MenuAction::ToggleRecording, Monotonic::ZERO);

    assert_eq!(host_button.calls(), host_menu.calls());
    assert_eq!(
        rt_button.view().pill.map(|p| p.status_label),
        rt_menu.view().pill.map(|p| p.status_label)
    );
}

#[test]
fn a_custom_hotkey_map_replaces_the_defaults() {
    let mut map = HotkeyMap::empty();
    let chord = Chord::new(Modifiers::CONTROL | Modifiers::OPTION, Key::Space);
    map.bind(chord, HotkeyAction::ToggleRecording).unwrap();

    let host = FakeHost::new();
    let mut rt = ShellRuntime::with_hotkeys(host.clone(), map);

    assert!(
        !rt.on_chord(toggle_chord(), Monotonic::ZERO),
        "default is gone"
    );
    assert!(rt.on_chord(chord, Monotonic::ZERO));
    assert_eq!(host.count(&HostCall::StartCapture), 1);
}

/// `libtest` runs every test on a spawned thread, so this *is* the wrong-thread
/// case. `NSApplication`, `NSStatusItem` and `NSPanel` are all main-thread-only
/// and AppKit aborts the process if they are touched from anywhere else, so the
/// guard has to turn that into a typed error before the first Objective-C
/// message is sent.
#[test]
fn starting_the_shell_off_the_main_thread_is_refused_not_fatal() {
    let err = fotw_shell::run(FakeHost::new(), HotkeyMap::defaults())
        .expect_err("the shell must refuse rather than abort");

    if cfg!(target_os = "macos") {
        assert!(matches!(err, ShellError::NotMainThread), "got {err}");
    } else {
        assert!(matches!(err, ShellError::Unsupported { .. }), "got {err}");
    }
}

#[test]
fn probing_the_shell_off_the_main_thread_is_refused_not_fatal() {
    let err = fotw_shell::platform::probe().expect_err("the probe must refuse rather than abort");

    if cfg!(target_os = "macos") {
        assert!(matches!(err, ShellError::NotMainThread), "got {err}");
    } else {
        assert!(matches!(err, ShellError::Unsupported { .. }), "got {err}");
    }
}

#[test]
fn the_notes_hotkey_does_not_touch_capture() {
    let (mut rt, host) = runtime();
    let notes = HotkeyMap::defaults()
        .chord_for(HotkeyAction::OpenNotes)
        .unwrap();

    assert!(rt.on_chord(notes, Monotonic::ZERO));
    assert_eq!(host.calls(), vec![HostCall::OpenNotes]);
    assert!(!rt.core().capture_is_live());
}

// --- detection arms, the user starts (CON-01) ----------------------------

fn zoom() -> DetectedMeeting {
    DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is holding the microphone")
        .with_title("Standup")
}

#[test]
fn a_detection_reaching_the_host_asks_it_to_do_nothing_at_all() {
    // The end-to-end version of CON-01: not "the core does not start", but
    // "the host — the thing that owns the tap — is never told to".
    let (mut rt, host) = runtime();

    rt.meeting_detected(Monotonic::ZERO, zoom());

    assert!(
        host.calls().is_empty(),
        "detection asked the host to do something: {:?}",
        host.calls()
    );
    let prompt = rt.view().prompt.expect("armed");
    assert!(prompt.headline.contains("Standup"));
    assert_eq!(prompt.start_label, "Start recording");
}

#[test]
fn a_host_that_reports_a_meeting_gets_a_prompt_and_nothing_else() {
    // The seam the daemon's detector arrives through, and the reason the
    // prompt panel can appear at all: `run()` owns the runtime, so a detector
    // living in the host has no other way to reach the screen (issue #52).
    let (mut rt, host) = runtime();
    host.report_detection(DetectionUpdate::Armed(zoom()));

    rt.tick(Monotonic::from_secs(1));

    let prompt = rt
        .view()
        .prompt
        .expect("the host's detection armed nothing");
    assert!(prompt.headline.contains("Standup"));
    assert!(
        host.calls().is_empty(),
        "a detection asked the host to do something: {:?}",
        host.calls()
    );
    assert!(!rt.core().capture_is_live());
}

#[test]
fn a_host_that_withdraws_a_meeting_takes_the_prompt_down() {
    let (mut rt, host) = runtime();
    host.report_detection(DetectionUpdate::Armed(zoom()));
    rt.tick(Monotonic::from_secs(1));
    assert!(rt.view().prompt.is_some());

    host.report_detection(DetectionUpdate::Cleared);
    rt.tick(Monotonic::from_secs(2));

    assert!(
        rt.view().prompt.is_none(),
        "the call ended and the prompt is still offering to record it"
    );
}

#[test]
fn a_host_repeating_the_same_detection_does_not_reset_the_acknowledgement() {
    // The pump polls at 20 Hz. A core that cleared the tick on every repeat
    // would make an all-party prompt impossible to start at all.
    let (mut rt, host) = runtime();
    let california = zoom().with_consent_notice("California — Cal. Penal Code § 632", true);
    for second in 0..5 {
        host.report_detection(DetectionUpdate::Armed(california.clone()));
        rt.tick(Monotonic::from_secs(second));
    }
    rt.acknowledge_prompt(true);
    for second in 5..10 {
        host.report_detection(DetectionUpdate::Armed(california.clone()));
        rt.tick(Monotonic::from_secs(second));
    }

    let prompt = rt.view().prompt.expect("armed");
    assert!(prompt.acknowledged, "the repeat un-ticked the box");
    assert!(prompt.start_enabled);
}

#[test]
fn the_prompt_start_button_reaches_the_host_with_a_consent_record() {
    let (mut rt, host) = runtime();
    rt.meeting_detected(Monotonic::ZERO, zoom());

    rt.respond_to_prompt(
        PromptChoice::Start {
            acknowledged: false,
        },
        Monotonic::from_secs(2),
    );

    assert_eq!(
        host.calls(),
        vec![
            HostCall::AuditStart(StartOrigin::DetectionPrompt),
            HostCall::StartCapture,
            HostCall::SetTicking(true),
        ]
    );
    assert!(rt.view().prompt.is_none());
}

#[test]
fn never_for_this_app_reaches_the_host_so_it_can_be_persisted() {
    let (mut rt, host) = runtime();
    rt.meeting_detected(Monotonic::ZERO, zoom());

    rt.respond_to_prompt(PromptChoice::NeverForThisApp, Monotonic::from_secs(1));

    assert_eq!(
        host.calls(),
        vec![HostCall::SuppressApp("us.zoom.xos".to_owned())],
        "a suppression the host never hears about is forgotten on quit"
    );
    assert!(!rt.core().capture_is_live());
}

#[test]
fn not_now_reaches_the_host_and_records_no_consent_event() {
    let (mut rt, host) = runtime();
    rt.meeting_detected(Monotonic::ZERO, zoom());

    rt.respond_to_prompt(PromptChoice::NotNow, Monotonic::from_secs(1));

    assert_eq!(host.calls(), vec![HostCall::SnoozeDetection]);
    assert!(
        !host
            .calls()
            .iter()
            .any(|c| matches!(c, HostCall::AuditStart(_))),
        "declining to record is not a start event"
    );
}

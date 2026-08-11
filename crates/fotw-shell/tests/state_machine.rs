//! The shell state machine, driven headless.
//!
//! These are the tests that would exist even if AppKit did not: every one of
//! them describes a decision the shell makes, not a pixel it draws.

use std::time::Duration;

use fotw_shell::{
    FINISHED_LINGER, Level, MenuAction, Monotonic, Phase, ShellCore, ShellEffect, ShellInput,
    TrayState, format_elapsed,
};

fn started_at(secs: u64) -> ShellCore {
    let mut core = ShellCore::new();
    core.handle(ShellInput::Start {
        at: Monotonic::from_secs(secs),
    });
    core
}

#[test]
fn idle_shows_nothing_and_offers_start() {
    let core = ShellCore::new();
    let view = core.view();

    assert!(view.pill.is_none(), "idle must not show a recording pill");
    assert_eq!(view.tray.state, TrayState::Idle);
    assert_eq!(
        view.tray.title, None,
        "no elapsed clock in the menu bar when idle"
    );
    assert_eq!(view.menu.record.label, "Start Recording");
    assert!(view.menu.record.enabled);
    assert_eq!(view.menu.status, "Not recording");
    assert!(!core.capture_is_live());
    assert!(
        !core.is_ticking(),
        "no timer is needed when nothing is on screen"
    );
}

#[test]
fn start_commands_capture_and_a_timer_exactly_once() {
    let mut core = ShellCore::new();
    let effects = core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
    });

    assert_eq!(
        effects,
        vec![ShellEffect::StartCapture, ShellEffect::StartTicking]
    );
    assert!(core.capture_is_live());
    assert!(core.is_ticking());
    assert!(core.view().pill.is_some());
}

#[test]
fn a_second_start_does_not_restart_the_session() {
    let mut core = started_at(0);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(90),
    });

    let effects = core.handle(ShellInput::Start {
        at: Monotonic::from_secs(90),
    });

    assert!(
        effects.is_empty(),
        "a second start must not re-enter the capture path"
    );
    assert_eq!(
        core.view().pill.unwrap().elapsed,
        Duration::from_secs(90),
        "the clock the user is watching must not reset"
    );
}

#[test]
fn elapsed_tracks_the_clock() {
    let mut core = started_at(1_000);
    for offset in [0u64, 1, 59, 60, 3599, 3600, 7325] {
        core.handle(ShellInput::Tick {
            now: Monotonic::from_secs(1_000 + offset),
        });
        assert_eq!(
            core.view().pill.unwrap().elapsed,
            Duration::from_secs(offset)
        );
    }
}

#[test]
fn a_backwards_clock_freezes_elapsed_rather_than_panicking() {
    let mut core = started_at(500);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(560),
    });
    assert_eq!(core.view().pill.unwrap().elapsed, Duration::from_secs(60));

    // A reading from before the session started. `Instant` subtraction would
    // panic here and a raw `now - started` would wrap; instead the display
    // holds at the largest value it has shown.
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(400),
    });
    assert_eq!(
        core.view().pill.unwrap().elapsed,
        Duration::from_secs(60),
        "a meeting timer that counts down is alarming and wrong"
    );

    // And it resumes from there once the clock recovers.
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(590),
    });
    assert_eq!(core.view().pill.unwrap().elapsed, Duration::from_secs(90));
}

#[test]
fn elapsed_label_switches_to_hours_at_the_hour() {
    let cases = [
        (0u64, "00:00"),
        (9, "00:09"),
        (59, "00:59"),
        (60, "01:00"),
        (754, "12:34"),
        (3_599, "59:59"),
        (3_600, "1:00:00"),
        (3_661, "1:01:01"),
        (7_325, "2:02:05"),
        (36_000, "10:00:00"),
    ];
    for (secs, expected) in cases {
        assert_eq!(
            format_elapsed(Duration::from_secs(secs)),
            expected,
            "{secs}s"
        );
    }
}

#[test]
fn stop_freezes_the_clock_and_disables_the_button() {
    let mut core = started_at(0);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(300),
    });

    let effects = core.handle(ShellInput::StopRequested);
    assert_eq!(effects, vec![ShellEffect::StopCapture]);
    assert!(!core.capture_is_live());
    assert!(
        core.is_ticking(),
        "the timer keeps running: the linger and the flush both need it"
    );

    let pill = core
        .view()
        .pill
        .expect("the session is still being written");
    assert_eq!(pill.status_label, "Finishing");
    assert!(
        !pill.stop_enabled,
        "a second stop must not re-enter teardown"
    );
    assert_eq!(pill.elapsed, Duration::from_secs(300));

    // The clock is frozen: capture ended, so the session cannot get longer.
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(400),
    });
    assert_eq!(core.view().pill.unwrap().elapsed, Duration::from_secs(300));
}

#[test]
fn a_second_stop_does_not_command_a_second_teardown() {
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);
    let effects = core.handle(ShellInput::StopRequested);
    assert!(effects.is_empty());
}

#[test]
fn capture_reporting_its_own_completion_does_not_command_a_stop() {
    let mut core = started_at(0);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(10),
    });

    let effects = core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(10),
    });

    assert!(
        !effects.contains(&ShellEffect::StopCapture),
        "the capture layer told us it had stopped; commanding it again tears down twice"
    );
    assert!(!core.capture_is_live());
    assert_eq!(core.view().pill.unwrap().status_label, "Saved");
}

#[test]
fn the_saved_pill_lingers_then_clears_itself() {
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);
    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(60),
    });

    // One tick short of the linger: still on screen.
    let almost = Monotonic::from_secs(60).plus(FINISHED_LINGER - Duration::from_millis(1));
    core.handle(ShellInput::Tick { now: almost });
    assert!(core.view().pill.is_some());
    assert_eq!(core.view().tray.state, TrayState::Idle);

    // Exactly the linger: gone.
    let expired = Monotonic::from_secs(60).plus(FINISHED_LINGER);
    let effects = core.handle(ShellInput::Tick { now: expired });
    assert_eq!(effects, vec![ShellEffect::StopTicking]);
    assert!(core.view().pill.is_none());
    assert_eq!(*core.phase(), Phase::Idle);
    assert!(!core.is_ticking());
}

#[test]
fn dismiss_clears_a_saved_session_early() {
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);
    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(60),
    });

    let effects = core.handle(ShellInput::Dismiss);
    assert_eq!(effects, vec![ShellEffect::StopTicking]);
    assert!(core.view().pill.is_none());
}

#[test]
fn a_fault_tears_capture_down_and_waits_to_be_acknowledged() {
    let mut core = started_at(0);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(120),
    });

    let effects = core.handle(ShellInput::CaptureFailed {
        reason: "default output device disappeared".to_owned(),
    });
    assert_eq!(effects, vec![ShellEffect::StopCapture]);

    let pill = core.view().pill.expect("a failed session must be reported");
    assert_eq!(pill.status_label, "Recording failed");
    assert!(!pill.stop_enabled);
    assert_eq!(core.view().tray.state, TrayState::Fault);
    assert!(
        core.view()
            .tray
            .tooltip
            .contains("default output device disappeared"),
        "the reason has to be reachable from the menu bar: {}",
        core.view().tray.tooltip
    );

    // A fault does NOT auto-clear. A user who believes a meeting was recorded
    // and finds nothing is the failure worth interrupting for.
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(120).plus(FINISHED_LINGER * 10),
    });
    assert!(
        core.view().pill.is_some(),
        "a fault must be acknowledged, not timed out"
    );

    core.handle(ShellInput::Dismiss);
    assert!(core.view().pill.is_none());
}

#[test]
fn toggle_during_teardown_is_dropped_not_queued() {
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);
    assert!(matches!(core.phase(), Phase::Finishing { .. }));

    let effects = core.toggle(Monotonic::from_secs(1));

    assert!(
        effects.is_empty(),
        "starting a capture on top of a teardown is how you get two taps on one device"
    );
    assert!(matches!(core.phase(), Phase::Finishing { .. }));
}

#[test]
fn a_start_arriving_during_teardown_is_refused() {
    // The toggle helper guards this too, but `Start` can arrive raw -- from the
    // web UI's consent sheet, or from a daemon replaying a queued request --
    // and the guard has to be in the state machine, not only in the helper.
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);

    let effects = core.handle(ShellInput::Start {
        at: Monotonic::from_secs(1),
    });

    assert!(
        effects.is_empty(),
        "starting a capture while the previous one is still flushing puts two \
         taps on one aggregate device"
    );
    assert!(matches!(core.phase(), Phase::Finishing { .. }));
    assert!(!core.capture_is_live());
}

#[test]
fn toggle_starts_from_idle_and_stops_from_recording() {
    let mut core = ShellCore::new();
    assert_eq!(
        core.toggle(Monotonic::ZERO),
        vec![ShellEffect::StartCapture, ShellEffect::StartTicking]
    );
    assert_eq!(
        core.toggle(Monotonic::from_secs(5)),
        vec![ShellEffect::StopCapture]
    );
}

#[test]
fn toggle_starts_a_new_session_from_saved_and_from_faulted() {
    for terminal in [false, true] {
        let mut core = started_at(0);
        if terminal {
            core.handle(ShellInput::CaptureFailed {
                reason: "x".to_owned(),
            });
        } else {
            core.handle(ShellInput::StopRequested);
            core.handle(ShellInput::StopCompleted {
                at: Monotonic::from_secs(1),
            });
        }

        let effects = core.toggle(Monotonic::from_secs(2));
        assert!(effects.contains(&ShellEffect::StartCapture));
        assert!(core.phase().is_recording());
        assert_eq!(
            core.view().pill.unwrap().elapsed,
            Duration::ZERO,
            "a new session starts a new clock"
        );
    }
}

#[test]
fn the_record_row_is_disabled_only_while_a_stop_is_in_flight() {
    let mut core = ShellCore::new();
    assert!(core.view().menu.record.enabled);

    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
    });
    assert_eq!(core.view().menu.record.label, "Stop Recording");
    assert!(core.view().menu.record.enabled);

    core.handle(ShellInput::StopRequested);
    assert!(
        !core.view().menu.record.enabled,
        "the stop is already running"
    );

    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(1),
    });
    assert_eq!(core.view().menu.record.label, "Start Recording");
    assert!(core.view().menu.record.enabled);
}

#[test]
fn a_click_on_a_disabled_row_does_nothing() {
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);

    let effects = core.on_menu(MenuAction::ToggleRecording, Monotonic::from_secs(1));

    assert!(effects.is_empty());
    assert!(matches!(core.phase(), Phase::Finishing { .. }));
}

#[test]
fn every_menu_row_dispatches_its_own_effect() {
    let cases = [
        (MenuAction::OpenNotes, ShellEffect::OpenNotes),
        (MenuAction::DisclosureKit, ShellEffect::OpenDisclosureKit),
        (MenuAction::Settings, ShellEffect::OpenSettings),
        (MenuAction::About, ShellEffect::OpenAbout),
        (MenuAction::Quit, ShellEffect::Quit),
    ];
    for (action, expected) in cases {
        let mut core = ShellCore::new();
        assert_eq!(core.on_menu(action, Monotonic::ZERO), vec![expected]);
    }
}

#[test]
fn quit_stays_available_while_recording() {
    let core = started_at(0);
    assert!(
        core.view().menu.quit.enabled,
        "refusing to quit traps the user in the state they are trying to leave"
    );
}

#[test]
fn the_menu_bar_shows_the_clock_only_while_a_session_is_open() {
    let mut core = started_at(0);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(754),
    });
    assert_eq!(core.view().tray.title.as_deref(), Some("12:34"));

    core.handle(ShellInput::StopRequested);
    assert_eq!(
        core.view().tray.title.as_deref(),
        Some("12:34"),
        "the clock stays up while the session is still being written"
    );

    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(754),
    });
    assert_eq!(core.view().tray.title, None);
}

#[test]
fn tray_state_is_distinct_in_every_phase_that_matters() {
    let mut core = ShellCore::new();
    assert_eq!(core.view().tray.state, TrayState::Idle);

    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
    });
    assert_eq!(core.view().tray.state, TrayState::Recording);

    core.handle(ShellInput::StopRequested);
    assert_eq!(core.view().tray.state, TrayState::Finishing);

    core.handle(ShellInput::CaptureFailed {
        reason: "x".to_owned(),
    });
    assert_eq!(core.view().tray.state, TrayState::Fault);
}

#[test]
fn the_level_meter_is_cleared_when_capture_ends() {
    let mut core = started_at(0);
    core.handle(ShellInput::Level(Level::new(0.9)));
    assert_eq!(core.view().pill.unwrap().level, Level::new(0.9));

    core.handle(ShellInput::StopRequested);
    assert_eq!(
        core.view().pill.unwrap().level,
        Level::SILENT,
        "a meter frozen on the last frame of a finished meeting reads as a live one"
    );
}

#[test]
fn a_level_arriving_after_capture_ended_is_ignored() {
    let mut core = started_at(0);
    core.handle(ShellInput::StopRequested);
    core.handle(ShellInput::Level(Level::new(1.0)));
    assert_eq!(core.view().pill.unwrap().level, Level::SILENT);
}

#[test]
fn level_clamps_its_input() {
    assert_eq!(Level::new(-3.0).get(), 0.0);
    assert_eq!(Level::new(17.0).get(), 1.0);
    assert_eq!(Level::new(0.25).get(), 0.25);
    assert_eq!(
        Level::new(f32::NAN),
        Level::SILENT,
        "an RMS over denormals can produce NaN; it must not poison the meter"
    );
    assert_eq!(Level::new(f32::INFINITY).get(), 1.0);
}

#[test]
fn the_meter_lights_a_segment_for_any_audible_signal() {
    assert_eq!(Level::SILENT.bars(6), 0);
    assert_eq!(
        Level::new(0.001).bars(6),
        1,
        "a meter reading empty during quiet speech is indistinguishable from a dead tap"
    );
    assert_eq!(Level::new(0.5).bars(6), 3);
    assert_eq!(Level::new(1.0).bars(6), 6);
    assert_eq!(Level::new(1.0).bars(0), 0);
    // Rounds *up*, not to nearest: a meter should over-report a signal rather
    // than under-report it, for the same reason as the case above.
    assert_eq!(Level::new(0.51).bars(6), 4);
    assert_eq!(Level::new(0.99).bars(6), 6);

    assert_eq!(Level::new(0.5).meter(6), "▮▮▮▯▯▯");
    assert_eq!(Level::SILENT.meter(4), "▯▯▯▯");
    assert_eq!(Level::new(0.5).meter(6).chars().count(), 6);
}

#[test]
fn phase_reports_elapsed_in_every_state() {
    let mut core = started_at(0);
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(42),
    });
    assert_eq!(core.phase().elapsed(), Duration::from_secs(42));

    core.handle(ShellInput::StopRequested);
    assert_eq!(core.phase().elapsed(), Duration::from_secs(42));

    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(43),
    });
    assert_eq!(core.phase().elapsed(), Duration::from_secs(42));

    assert_eq!(ShellCore::new().phase().elapsed(), Duration::ZERO);
}

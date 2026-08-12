//! CON-02: the recording indicator is not suppressible.
//!
//! > **Non-dismissable recording indicator** — menu-bar item in a distinct
//! > state plus a small always-on-top pill with elapsed time, level meters and
//! > a Stop button. **No build flag, preference, or CLI arg suppresses it.**
//! >
//! > — docs/REQUIREMENTS.md 11.2
//!
//! This file is the executable half of that requirement. It cannot prove the
//! panel is on screen — no CI runner has a window server — but it can prove
//! that the state machine which decides whether it is on screen has no path
//! to "off while recording", and that no build flag exists to add one. What
//! is left is a `QA.md` line item, not a test that quietly passes.

use std::path::Path;
use std::time::Duration;

use fotw_shell::testing::Rng;
use fotw_shell::{
    DetectedMeeting, Level, MenuAction, Monotonic, PromptChoice, ShellCore, ShellEffect,
    ShellInput, StartOrigin, TrayState,
};

/// One of every [`ShellInput`] variant.
///
/// The `match` below has no wildcard arm, so **adding an input variant breaks
/// this file's compilation** until the author extends the list. That is the
/// point: a "hide the indicator" input must not be able to slip in behind a
/// `_ => {}`.
fn every_input(now: Monotonic) -> Vec<ShellInput> {
    fn assert_exhaustive(input: &ShellInput) {
        match input {
            ShellInput::Start { .. }
            | ShellInput::Tick { .. }
            | ShellInput::Level(_)
            | ShellInput::StopRequested
            | ShellInput::StopCompleted { .. }
            | ShellInput::CaptureFailed { .. }
            | ShellInput::Dismiss
            | ShellInput::MeetingDetected { .. }
            | ShellInput::DetectionCleared
            | ShellInput::PromptResponse { .. } => {}
        }
    }

    let all = vec![
        ShellInput::Start {
            at: now,
            origin: StartOrigin::Menu,
        },
        ShellInput::Tick { now },
        ShellInput::Level(Level::new(0.5)),
        ShellInput::StopRequested,
        ShellInput::StopCompleted { at: now },
        ShellInput::CaptureFailed {
            reason: "device disappeared".to_owned(),
        },
        ShellInput::Dismiss,
        ShellInput::MeetingDetected {
            at: now,
            meeting: DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is holding the microphone"),
        },
        ShellInput::DetectionCleared,
        ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::Start { acknowledged: true },
        },
        ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::NotNow,
        },
        ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::NeverForThisApp,
        },
    ];
    for input in &all {
        assert_exhaustive(input);
    }
    all
}

#[test]
fn no_input_takes_the_indicator_down_while_a_session_is_open() {
    for input in every_input(Monotonic::from_secs(30)) {
        let mut core = ShellCore::new();
        core.handle(ShellInput::Start {
            at: Monotonic::ZERO,
            origin: StartOrigin::Menu,
        });
        core.handle(ShellInput::Tick {
            now: Monotonic::from_secs(30),
        });
        assert!(core.capture_is_live());

        core.handle(input.clone());

        assert!(
            core.view().pill.is_some(),
            "{input:?} took the recording indicator down in one step"
        );
    }
}

#[test]
fn the_indicator_covers_the_whole_capture_window_and_then_some() {
    let mut core = ShellCore::new();
    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
        origin: StartOrigin::Menu,
    });

    // Live.
    assert!(core.capture_is_live());
    assert!(core.view().pill.is_some());

    // Stop pressed. Capture is over but the session is still being written to
    // disk, and the indicator has to cover that too.
    core.handle(ShellInput::StopRequested);
    assert!(!core.capture_is_live());
    assert!(
        core.view().pill.is_some(),
        "bytes from the meeting are still being written"
    );
    assert_eq!(core.view().tray.state, TrayState::Finishing);

    // Written and closed. Still shown, briefly, so the user sees it happened.
    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(1),
    });
    assert!(core.view().pill.is_some());
}

#[test]
fn the_pill_always_carries_elapsed_time_a_meter_and_a_stop_button() {
    // CON-02 names all three. A pill missing any of them is a badge.
    let mut core = ShellCore::new();
    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
        origin: StartOrigin::Menu,
    });
    core.handle(ShellInput::Level(Level::new(0.7)));
    core.handle(ShellInput::Tick {
        now: Monotonic::from_secs(3_723),
    });

    let pill = core.view().pill.expect("recording shows an indicator");
    assert_eq!(pill.elapsed_label, "1:02:03");
    assert_eq!(pill.elapsed, Duration::from_secs(3_723));
    assert!(pill.level.bars(6) > 0, "the meter must reflect the signal");
    assert!(pill.stop_enabled, "the Stop button must be reachable");
    assert!(pill.accessibility_label().contains("1:02:03"));
    assert!(pill.accessibility_label().contains("Recording"));
}

#[test]
fn the_menu_bar_item_is_in_a_distinct_state_while_recording() {
    let mut core = ShellCore::new();
    let idle = core.view().tray;
    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
        origin: StartOrigin::Menu,
    });
    let recording = core.view().tray;

    assert_ne!(idle.state, recording.state);
    assert_ne!(idle.tooltip, recording.tooltip);
    assert!(
        recording.title.is_some() && idle.title.is_none(),
        "the elapsed clock in the menu bar is the second, independent surface"
    );
}

#[test]
fn no_menu_row_suppresses_the_indicator() {
    assert_eq!(
        MenuAction::ALL.len(),
        6,
        "a new menu row was added; confirm it does not hide the indicator"
    );
    for action in MenuAction::ALL {
        let id = action.id();
        assert!(
            !id.contains("hide") && !id.contains("stealth") && !id.contains("suppress"),
            "menu row {id} looks like a suppression control"
        );
        assert_eq!(MenuAction::from_id(id), Some(action), "id round-trip: {id}");
    }
}

#[test]
fn the_crate_declares_no_build_flags_at_all() {
    let manifest = include_str!("../Cargo.toml");
    // Table headers only -- the manifest is allowed to *explain* the rule in
    // a comment, which is why this is not a substring search.
    let declares_features = manifest
        .lines()
        .any(|line| line.trim_start().starts_with("[features]"));
    assert!(
        !declares_features,
        "fotw-shell must have no cargo features: the cheapest way to guarantee \
         no build flag suppresses the indicator is to have no build flags"
    );
}

#[test]
fn no_source_file_offers_a_way_to_suppress_the_indicator() {
    // Identifier shapes, not prose: the docs are allowed to *discuss* the
    // requirement, but no function may implement its opposite.
    const BANNED: [&str; 6] = [
        "fn hide",
        "hide_pill",
        "hide_indicator",
        "set_indicator_visible",
        "suppress_indicator",
        "stealth",
    ];

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0usize;
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("src must be readable") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("source must be readable");
            for needle in BANNED {
                assert!(
                    !body.contains(needle),
                    "{} contains {needle:?}",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 10,
        "expected to scan the whole crate, only saw {checked} files -- the walk is broken"
    );
}

// --- the invariant sweep -------------------------------------------------

fn random_input(rng: &mut Rng, now: Monotonic) -> ShellInput {
    // Ticks are weighted heavily so linger expiry is actually reached.
    match rng.below(16) {
        0 => ShellInput::Start {
            at: now,
            origin: StartOrigin::Menu,
        },
        1..=4 => ShellInput::Tick { now },
        5 => {
            #[expect(clippy::cast_precision_loss, reason = "value is at most 100")]
            let level = rng.below(101) as f32 / 100.0;
            ShellInput::Level(Level::new(level))
        }
        6 | 7 => ShellInput::StopRequested,
        8 | 9 => ShellInput::StopCompleted { at: now },
        10 => ShellInput::CaptureFailed {
            reason: "aggregate device vanished".to_owned(),
        },
        11 => ShellInput::Dismiss,
        // The detection inputs are swept here too: an armed prompt must not
        // be able to interfere with the indicator of a live recording.
        12 | 13 => ShellInput::MeetingDetected {
            at: now,
            meeting: DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is holding the microphone"),
        },
        14 => ShellInput::DetectionCleared,
        _ => ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::Start { acknowledged: true },
        },
    }
}

#[test]
fn indicator_invariants_hold_over_two_hundred_thousand_random_inputs() {
    let mut rng = Rng::new(0x_F01D_C0FF_EE00_2602);

    for run in 0..1_000u32 {
        let mut core = ShellCore::new();
        let mut clock: u64 = 0;

        for step in 0..200u32 {
            let where_ = format!("run {run} step {step}");

            // A clock that mostly advances and occasionally jumps backwards,
            // because real ones do.
            if rng.below(25) == 0 {
                clock = clock.saturating_sub(rng.below(5_000));
            } else {
                clock = clock.saturating_add(rng.below(3_000));
            }
            let now = Monotonic::from_millis(clock);

            let input = random_input(&mut rng, now);
            let was_live = core.capture_is_live();
            let was_idle = core.phase().is_idle();
            let was_elapsed = core.phase().elapsed();

            let effects = core.handle(input.clone());
            let view = core.view();

            // 1. CON-02, stated at its strongest: if the host has been told to
            //    capture and not told to stop, something is on screen.
            if core.capture_is_live() {
                assert!(
                    view.pill.is_some(),
                    "{where_}: capture live with no indicator after {input:?}"
                );
            }

            // 2. The pill is shown exactly when there is something to say.
            assert_eq!(
                view.pill.is_some(),
                !core.phase().is_idle(),
                "{where_}: pill visibility disagrees with the phase after {input:?}"
            );

            // 3. A timer runs exactly while something is on screen. Without
            //    this the elapsed clock silently stops advancing.
            assert_eq!(
                core.is_ticking(),
                !core.phase().is_idle(),
                "{where_}: tick state disagrees with the phase after {input:?}"
            );

            // 4. Capture commands are edges, never repeated or contradictory.
            let starts = effects
                .iter()
                .filter(|e| **e == ShellEffect::StartCapture)
                .count();
            let stops = effects
                .iter()
                .filter(|e| **e == ShellEffect::StopCapture)
                .count();
            assert!(
                starts <= 1 && stops <= 1,
                "{where_}: duplicate capture command"
            );
            assert!(
                starts == 0 || stops == 0,
                "{where_}: start and stop in the same step"
            );
            assert_eq!(
                starts == 1,
                !was_live && core.capture_is_live(),
                "{where_}: StartCapture does not match the capture edge"
            );
            if stops == 1 {
                assert!(
                    was_live && !core.capture_is_live(),
                    "{where_}: StopCapture without a live capture"
                );
            }
            // The only way capture ends without a StopCapture is the capture
            // layer reporting its own completion.
            if was_live && !core.capture_is_live() && stops == 0 {
                assert!(
                    matches!(input, ShellInput::StopCompleted { .. }),
                    "{where_}: capture ended silently after {input:?}"
                );
            }

            // 5. Elapsed never runs backwards within one session. This is the
            //    invariant a naive `now - started` breaks the first time a
            //    clock reading arrives out of order. A `Start` legitimately
            //    zeroes it, so those steps are excluded.
            let starting_new_session = matches!(input, ShellInput::Start { .. });
            if !was_idle && !core.phase().is_idle() && !starting_new_session {
                assert!(
                    core.phase().elapsed() >= was_elapsed,
                    "{where_}: elapsed went backwards, {:?} -> {:?}, after {input:?}",
                    was_elapsed,
                    core.phase().elapsed()
                );
            }

            // 6. The menu-bar item is only in the recording state when it is
            //    actually recording.
            assert_eq!(
                view.tray.state == TrayState::Recording,
                core.phase().is_recording(),
                "{where_}: menu-bar state misreports the phase"
            );

            // 7. The record row offers "Stop" exactly while a session is open,
            //    and is clickable only while it can actually be stopped.
            let session_open =
                view.tray.state == TrayState::Recording || view.tray.state == TrayState::Finishing;
            assert_eq!(
                view.menu.record.label == "Stop Recording",
                session_open,
                "{where_}: record row label disagrees with the phase"
            );
            assert_eq!(
                view.menu.record.enabled,
                view.tray.state != TrayState::Finishing,
                "{where_}: record row enablement disagrees with the phase"
            );
        }
    }
}

//! CON-01: **detection arms, the user starts.** Nothing else may start capture.
//!
//! > **Never ship silent auto-record.** Default is *arm and prompt*. […]
//! > Acceptance: a fresh install never writes an audio buffer to disk without
//! > a user-initiated Start event in the local audit log.
//! >
//! > — docs/REQUIREMENTS.md 11.2 CON-01, and issue #22
//!
//! This is not a UX preference. Starting capture from a detection signal is
//! the specific conduct pleaded in *Chamberlain v. Granola* (N.D. Cal.
//! 3:26-cv-07926-EMC), which asks $5,000 per recorded participant under CIPA
//! § 637.2(a). A code path that begins capture without a human pressing
//! something is the highest-severity defect this repository can contain, so
//! this file tries hard to find one:
//!
//! - every input variant is enumerated with no wildcard arm, so a new one
//!   cannot slip past unexamined;
//! - the only inputs allowed to produce `StartCapture` are named explicitly,
//!   and every other input is driven from every reachable phase;
//! - a randomised sweep asserts the same invariant over 200,000 steps,
//!   including the ordering rule that makes the audit log trustworthy.

use fotw_shell::testing::Rng;
use fotw_shell::{
    DetectedMeeting, Level, Monotonic, PromptChoice, ShellCore, ShellEffect, ShellInput,
    StartOrigin,
};

fn zoom() -> DetectedMeeting {
    DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is holding the microphone")
}

/// One of every [`ShellInput`] variant.
///
/// The `match` has no wildcard arm: **adding an input breaks this file's
/// compilation** until its author decides whether the new input may start a
/// recording. That is the entire point.
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
            meeting: zoom(),
        },
        ShellInput::DetectionCleared,
        ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::Start {
                acknowledged: false,
            },
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

/// Whether an input represents a human deliberately asking to record.
///
/// Exactly two do. `Start` is the menu row, the hotkey and the web UI;
/// `PromptResponse { Start }` is the button on the detection prompt. Detection
/// itself is not on this list and must never be.
fn is_a_human_pressing_record(input: &ShellInput) -> bool {
    matches!(
        input,
        ShellInput::Start { .. }
            | ShellInput::PromptResponse {
                choice: PromptChoice::Start { .. },
                ..
            }
    )
}

#[test]
fn detection_alone_never_starts_capture() {
    let mut core = ShellCore::new();

    let effects = core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: zoom(),
    });

    assert!(
        !effects.contains(&ShellEffect::StartCapture),
        "detection commanded capture: this is the Granola complaint, in code"
    );
    assert!(!core.capture_is_live());
    assert!(
        core.view().pill.is_none(),
        "nothing is being recorded, so nothing may indicate that it is"
    );
    // It arms: the affordance is on screen, waiting for a person.
    let prompt = core.view().prompt.expect("detection must raise a prompt");
    assert!(prompt.headline.contains("Zoom"));
    assert!(prompt.evidence.contains("microphone"));
}

#[test]
fn no_input_except_a_human_pressing_record_can_start_capture() {
    let now = Monotonic::from_secs(10);
    for input in every_input(now) {
        if is_a_human_pressing_record(&input) {
            continue;
        }
        // From idle, and from idle-with-a-prompt-up: the two states from
        // which a rogue path could begin a recording.
        for armed in [false, true] {
            let mut core = ShellCore::new();
            if armed {
                core.handle(ShellInput::MeetingDetected {
                    at: Monotonic::ZERO,
                    meeting: zoom(),
                });
            }
            let effects = core.handle(input.clone());
            assert!(
                !effects.contains(&ShellEffect::StartCapture),
                "{input:?} started capture with no human action (armed={armed})"
            );
            assert!(
                !core.capture_is_live(),
                "{input:?} left capture live with no human action (armed={armed})"
            );
        }
    }
}

#[test]
fn repeated_detection_never_accumulates_into_a_start() {
    // The realistic attack on this property is not one bad input, it is a
    // detector that fires every few seconds for an hour.
    let mut core = ShellCore::new();
    for tick in 0..1_000u64 {
        let at = Monotonic::from_secs(tick);
        let effects = core.handle(ShellInput::MeetingDetected {
            at,
            meeting: zoom(),
        });
        assert!(!effects.contains(&ShellEffect::StartCapture));
        core.handle(ShellInput::Tick { now: at });
    }
    assert!(
        !core.capture_is_live(),
        "1,000 detections started a recording"
    );
}

#[test]
fn the_prompt_start_button_does_start_capture() {
    // The other half of the requirement: arming must actually be reachable,
    // or the feature is a no-op that trivially satisfies every test above.
    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: zoom(),
    });

    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(3),
        choice: PromptChoice::Start {
            acknowledged: false,
        },
    });

    assert!(effects.contains(&ShellEffect::StartCapture));
    assert!(core.capture_is_live());
    assert!(core.view().pill.is_some(), "CON-02: recording shows a pill");
    assert!(
        core.view().prompt.is_none(),
        "the prompt is answered and must come down"
    );
}

#[test]
fn a_start_response_with_no_prompt_on_screen_does_nothing() {
    // A response can arrive for a prompt that has already been withdrawn --
    // the call ended, the detector cleared, the click was in flight. Honouring
    // it would start a recording nobody is looking at.
    let mut core = ShellCore::new();
    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::ZERO,
        choice: PromptChoice::Start { acknowledged: true },
    });
    assert!(effects.is_empty());
    assert!(!core.capture_is_live());

    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: zoom(),
    });
    core.handle(ShellInput::DetectionCleared);
    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::Start { acknowledged: true },
    });
    assert!(
        effects.is_empty(),
        "a response to a withdrawn prompt started a recording"
    );
    assert!(!core.capture_is_live());
}

#[test]
fn every_start_origin_is_a_human_action() {
    // There is deliberately no `Automatic` variant. If one is ever added, this
    // test is where the author has to argue for it.
    assert_eq!(StartOrigin::ALL.len(), 5);
    for origin in StartOrigin::ALL {
        assert!(
            origin.is_human(),
            "{origin:?} is not a human action but can start a recording"
        );
        assert!(!format!("{origin:?}").to_lowercase().contains("auto"));
    }
}

#[test]
fn a_blocking_jurisdiction_warning_cannot_be_clicked_past() {
    // CON-05: an all-party or contested jurisdiction escalates the pre-record
    // sheet to a blocking modal requiring an explicit acknowledgement. The
    // warning riding on this surface is the reason detection false positives
    // are a consent problem: a user habituated into dismissing this prompt
    // stops reading the warning too.
    let meeting = zoom().with_consent_notice(
        "California requires every participant's consent — Cal. Penal Code § 632",
        true,
    );
    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting,
    });

    let prompt = core.view().prompt.expect("armed");
    assert!(prompt.requires_acknowledgement);
    assert!(prompt.consent_notice.contains("§ 632"));

    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::Start {
            acknowledged: false,
        },
    });
    assert!(
        !effects.contains(&ShellEffect::StartCapture),
        "started recording without the all-party acknowledgement"
    );
    assert!(!core.capture_is_live());
    assert!(
        core.view().prompt.is_some(),
        "the prompt stays up so the user can acknowledge"
    );

    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(2),
        choice: PromptChoice::Start { acknowledged: true },
    });
    assert!(effects.contains(&ShellEffect::StartCapture));
}

#[test]
fn not_now_and_never_report_the_users_intent_to_the_detector() {
    // Issue #22 asks for per-app "never detect from this app" suppression, and
    // "not now" has to mean something or the prompt reappears in four seconds.
    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: zoom(),
    });
    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::NotNow,
    });
    assert_eq!(effects, vec![ShellEffect::SnoozeDetection]);
    assert!(core.view().prompt.is_none());

    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: zoom(),
    });
    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::NeverForThisApp,
    });
    assert_eq!(
        effects,
        vec![ShellEffect::SuppressApp("us.zoom.xos".to_owned())]
    );
    assert!(core.view().prompt.is_none());
}

#[test]
fn detection_is_ignored_while_a_session_is_already_open() {
    // Prompting during a recording is noise, and a prompt on top of a live
    // session invites a click that would do nothing.
    let mut core = ShellCore::new();
    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
        origin: StartOrigin::Menu,
    });
    let effects = core.handle(ShellInput::MeetingDetected {
        at: Monotonic::from_secs(1),
        meeting: zoom(),
    });
    assert!(effects.is_empty());
    assert!(core.view().prompt.is_none());

    core.handle(ShellInput::StopRequested);
    let effects = core.handle(ShellInput::MeetingDetected {
        at: Monotonic::from_secs(2),
        meeting: zoom(),
    });
    assert!(
        effects.is_empty() && core.view().prompt.is_none(),
        "a prompt during teardown would race the next session"
    );
}

#[test]
fn every_start_is_immediately_preceded_by_an_audit_record() {
    // CON-01's acceptance criterion is about the audit log, not the UI: a
    // fresh install never writes an audio buffer without a *recorded*
    // user-initiated Start. Emitting the audit effect first, in the same
    // batch, is what makes the log complete even if capture then fails.
    for (input, expected) in [
        (
            ShellInput::Start {
                at: Monotonic::ZERO,
                origin: StartOrigin::Hotkey,
            },
            StartOrigin::Hotkey,
        ),
        (
            ShellInput::PromptResponse {
                at: Monotonic::ZERO,
                choice: PromptChoice::Start { acknowledged: true },
            },
            StartOrigin::DetectionPrompt,
        ),
    ] {
        let mut core = ShellCore::new();
        core.handle(ShellInput::MeetingDetected {
            at: Monotonic::ZERO,
            meeting: zoom(),
        });
        let effects = core.handle(input);
        let audit = effects
            .iter()
            .position(|e| *e == ShellEffect::AuditStart(expected))
            .expect("a start with no audit record");
        let start = effects
            .iter()
            .position(|e| *e == ShellEffect::StartCapture)
            .expect("start");
        assert!(audit < start, "the audit record must precede capture");
    }
}

// --- the sweep -----------------------------------------------------------

fn random_input(rng: &mut Rng, now: Monotonic) -> ShellInput {
    match rng.below(16) {
        0 => ShellInput::Start {
            at: now,
            origin: StartOrigin::Menu,
        },
        1..=3 => ShellInput::Tick { now },
        4 => ShellInput::Level(Level::new(0.5)),
        5 => ShellInput::StopRequested,
        6 => ShellInput::StopCompleted { at: now },
        7 => ShellInput::CaptureFailed {
            reason: "aggregate device vanished".to_owned(),
        },
        8 => ShellInput::Dismiss,
        9..=11 => ShellInput::MeetingDetected {
            at: now,
            meeting: if rng.below(2) == 0 {
                zoom()
            } else {
                zoom().with_consent_notice("all-party jurisdiction", true)
            },
        },
        12 => ShellInput::DetectionCleared,
        13 => ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::Start {
                acknowledged: rng.below(2) == 0,
            },
        },
        14 => ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::NotNow,
        },
        _ => ShellInput::PromptResponse {
            at: now,
            choice: PromptChoice::NeverForThisApp,
        },
    }
}

#[test]
fn capture_never_begins_without_a_human_over_two_hundred_thousand_random_inputs() {
    let mut rng = Rng::new(0x_C0_1D_C0_FF_EE_00_22_31);

    for run in 0..1_000u32 {
        let mut core = ShellCore::new();
        let mut clock: u64 = 0;

        for step in 0..200u32 {
            let where_ = format!("run {run} step {step}");
            clock = clock.saturating_add(rng.below(3_000));
            let now = Monotonic::from_millis(clock);

            let input = random_input(&mut rng, now);
            let armed_before = core.view().prompt.clone();
            let effects = core.handle(input.clone());

            let started = effects.contains(&ShellEffect::StartCapture);
            if started {
                assert!(
                    is_a_human_pressing_record(&input),
                    "{where_}: {input:?} started capture"
                );

                // The audit record is emitted first, and names a human.
                let audit_at = effects
                    .iter()
                    .position(|e| matches!(e, ShellEffect::AuditStart(_)))
                    .unwrap_or_else(|| panic!("{where_}: start with no audit record"));
                let start_at = effects
                    .iter()
                    .position(|e| *e == ShellEffect::StartCapture)
                    .expect("start");
                assert!(audit_at < start_at, "{where_}: audit record came late");
                match effects[audit_at] {
                    ShellEffect::AuditStart(origin) => assert!(origin.is_human()),
                    _ => unreachable!(),
                }

                // A blocking consent warning is never started past.
                if let ShellInput::PromptResponse {
                    choice: PromptChoice::Start { acknowledged },
                    ..
                } = &input
                {
                    let prompt = armed_before
                        .as_ref()
                        .unwrap_or_else(|| panic!("{where_}: started from no prompt"));
                    assert!(
                        *acknowledged || !prompt.requires_acknowledgement,
                        "{where_}: clicked past a blocking jurisdiction warning"
                    );
                }
            }

            // A prompt is never on screen at the same time as a recording:
            // one surface, one decision.
            let view = core.view();
            assert!(
                !(view.prompt.is_some() && core.capture_is_live()),
                "{where_}: prompt shown over a live recording"
            );
        }
    }
}

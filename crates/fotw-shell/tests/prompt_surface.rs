//! The detection prompt as a surface a person can actually see and click.
//!
//! `tests/con01_detection_arms_only.rs` proves the state machine never records
//! without a human. This file proves the *other half*, which is what issue #52
//! is about: the prompt has to be drawable, and the all-party acknowledgement
//! (CON-05) has to be part of the drawn thing rather than a flag some renderer
//! is trusted to ask about.
//!
//! The load-bearing test here is
//! [`the_drawn_enablement_never_disagrees_with_the_state_machine`]. The panel
//! greys its Start button out from [`PromptView::start_enabled`], and the core
//! independently refuses a start that has not been acknowledged. Two rules
//! that can drift apart is how a user in California ends up with a clickable
//! Start button that silently does nothing — or, in the direction that
//! matters, an enabled one that records.

use fotw_shell::{
    DetectedMeeting, Level, Monotonic, PromptChoice, PromptView, ShellCore, ShellEffect,
    ShellInput, StartOrigin,
};

fn zoom() -> DetectedMeeting {
    DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is using the microphone")
}

/// What `fotwd`'s consent engine hands back for the shipped default home
/// jurisdiction — `US-CA`, which is all-party. This is not an edge case: it is
/// what every prompt looks like on a fresh install.
fn california() -> DetectedMeeting {
    zoom().with_consent_notice(
        "These jurisdictions require every participant's consent:\n  \
         • California — Cal. Penal Code § 632 (https://leginfo.legislature.ca.gov/faces/\
         codes_displaySection.xhtml?lawCode=PEN&sectionNum=632)\nThis is not legal advice.",
        true,
    )
}

fn armed(meeting: DetectedMeeting) -> ShellCore {
    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting,
    });
    core
}

fn prompt(core: &ShellCore) -> PromptView {
    core.view().prompt.expect("a prompt must be on screen")
}

#[test]
fn the_prompt_view_carries_everything_the_panel_has_to_draw() {
    // A panel cannot render a field that is not in the view, and the renderer
    // has no other source for any of this.
    let view = prompt(&armed(california().with_title("Design review")));

    assert_eq!(view.app_key, "us.zoom.xos");
    assert!(view.headline.contains("Design review"));
    assert!(view.headline.contains("Zoom"));
    assert!(view.evidence.contains("microphone"));
    assert!(view.consent_notice.contains("§ 632"));
    assert!(view.requires_acknowledgement);

    // Three buttons and a checkbox, all labelled. Issue #22 names the three.
    for label in [
        view.start_label,
        view.not_now_label,
        view.never_label,
        view.acknowledge_label,
    ] {
        assert!(
            !label.is_empty(),
            "an unlabelled control is an unusable one"
        );
    }
    assert!(
        view.start_label.to_lowercase().contains("record"),
        "the affirmative button must say what it does, not `Yes`"
    );
}

#[test]
fn the_accessibility_label_reads_the_jurisdiction_warning_too() {
    // A blind user must hear the warning, not just the headline. The panel
    // hands this one string to VoiceOver.
    let view = prompt(&armed(california()));
    let label = view.accessibility_label();
    assert!(label.contains("Zoom"));
    assert!(label.contains("microphone"));
    assert!(
        label.contains("§ 632"),
        "the consent notice must be spoken: {label}"
    );

    // With nothing to warn about there is no dangling separator.
    let plain = prompt(&armed(zoom())).accessibility_label();
    assert!(!plain.ends_with(' ') && !plain.ends_with('—'));
}

#[test]
fn start_is_drawn_disabled_until_the_all_party_box_is_ticked() {
    let mut core = armed(california());
    assert!(
        !prompt(&core).start_enabled,
        "CON-05: an all-party jurisdiction must not offer a live Start button"
    );
    assert!(!prompt(&core).acknowledged);

    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    assert!(prompt(&core).acknowledged);
    assert!(prompt(&core).start_enabled);

    // And it un-ticks.
    core.handle(ShellInput::PromptAcknowledged {
        acknowledged: false,
    });
    assert!(!prompt(&core).acknowledged);
    assert!(!prompt(&core).start_enabled);
}

#[test]
fn a_one_party_prompt_is_startable_immediately() {
    // The warning is a reminder there, not a gate. Making every prompt
    // blocking would habituate the tick, which is the failure CON-05 is
    // trying to avoid.
    let view = prompt(&armed(zoom().with_consent_notice(
        "Everyone on this call should know they're being recorded.",
        false,
    )));
    assert!(!view.requires_acknowledgement);
    assert!(view.start_enabled);
    assert!(!view.acknowledged);
}

#[test]
fn the_drawn_enablement_never_disagrees_with_the_state_machine() {
    // The panel greys Start out from `start_enabled`; the core refuses an
    // unacknowledged start on its own. If those two rules ever disagree, one
    // of them is wrong -- and the dangerous direction (an enabled button the
    // core refuses, or worse, a disabled-looking gate the core honours) is
    // invisible to every other test in the suite.
    for meeting in [zoom(), california()] {
        for tick in [false, true] {
            let mut core = armed(meeting.clone());
            core.handle(ShellInput::PromptAcknowledged { acknowledged: tick });

            let view = prompt(&core);
            // Exactly what the renderer sends when the button is pressed: the
            // state of the checkbox it drew.
            let effects = core.handle(ShellInput::PromptResponse {
                at: Monotonic::from_secs(1),
                choice: PromptChoice::Start {
                    acknowledged: view.acknowledged,
                },
            });

            assert_eq!(
                view.start_enabled,
                effects.contains(&ShellEffect::StartCapture),
                "drawn enablement disagrees with the state machine \
                 (blocking={}, ticked={tick})",
                meeting.requires_acknowledgement
            );
            assert_eq!(
                view.start_enabled,
                core.capture_is_live(),
                "a disabled Start still started capture (ticked={tick})"
            );
        }
    }
}

#[test]
fn the_box_on_screen_counts_even_when_the_click_does_not_carry_it() {
    // Two surfaces can supply the acknowledgement: the checkbox this core is
    // tracking, and a caller that asserts it in the click (the web UI, which
    // draws its own). A core that only honoured the second would leave the
    // panel's own checkbox decorative -- Start would grey in, and then do
    // nothing when pressed.
    let mut core = armed(california());
    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });

    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::Start {
            acknowledged: false,
        },
    });
    assert!(
        effects.contains(&ShellEffect::StartCapture),
        "the ticked box on screen was ignored"
    );

    // And the converse: a click that carries it works with no box ticked,
    // because that caller drew its own.
    let mut core = armed(california());
    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::Start { acknowledged: true },
    });
    assert!(effects.contains(&ShellEffect::StartCapture));

    // Neither one, and nothing happens.
    let mut core = armed(california());
    let effects = core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::Start {
            acknowledged: false,
        },
    });
    assert!(!effects.contains(&ShellEffect::StartCapture));
}

#[test]
fn the_tick_does_not_outlive_the_prompt_it_was_given_for() {
    // A stale acknowledgement is a forged one: it says a person agreed that
    // everyone on *this* call consented, about a call they were never shown.

    // Withdrawn, then re-armed by the same app.
    let mut core = armed(california());
    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    core.handle(ShellInput::DetectionCleared);
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::from_secs(60),
        meeting: california(),
    });
    assert!(
        !prompt(&core).acknowledged,
        "a tick survived the prompt being withdrawn"
    );

    // Answered "not now", then armed again.
    let mut core = armed(california());
    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::NotNow,
    });
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::from_secs(600),
        meeting: california(),
    });
    assert!(!prompt(&core).acknowledged, "a tick survived `Not now`");

    // Started, recorded, finished -- and the next meeting starts clean.
    let mut core = armed(california());
    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    core.handle(ShellInput::PromptResponse {
        at: Monotonic::from_secs(1),
        choice: PromptChoice::Start { acknowledged: true },
    });
    core.handle(ShellInput::StopRequested);
    core.handle(ShellInput::StopCompleted {
        at: Monotonic::from_secs(2),
    });
    core.handle(ShellInput::Dismiss);
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::from_secs(3),
        meeting: california(),
    });
    assert!(
        !prompt(&core).acknowledged,
        "a tick survived into the next meeting"
    );
    assert!(!prompt(&core).start_enabled);

    // A *different* meeting replacing the one on screen.
    let mut core = armed(california());
    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::from_secs(5),
        meeting: california().with_title("Someone else's call"),
    });
    assert!(
        !prompt(&core).acknowledged,
        "a tick for one meeting carried over to another"
    );
}

#[test]
fn a_detector_repeating_itself_does_not_clear_the_tick_under_the_cursor() {
    // The detector holds rather than re-arming, but the core must not depend
    // on that: a prompt that un-ticks itself while the user reaches for Start
    // can never be started at all.
    let mut core = armed(california());
    core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    for second in 1..50u64 {
        core.handle(ShellInput::MeetingDetected {
            at: Monotonic::from_secs(second),
            meeting: california(),
        });
        core.handle(ShellInput::Tick {
            now: Monotonic::from_secs(second),
        });
        core.handle(ShellInput::Level(Level::new(0.4)));
    }
    assert!(prompt(&core).acknowledged, "the identical prompt re-ticked");
    assert!(prompt(&core).start_enabled);
}

#[test]
fn acknowledging_with_no_prompt_on_screen_does_nothing_at_all() {
    let mut core = ShellCore::new();
    let effects = core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    assert!(effects.is_empty());
    assert!(core.view().prompt.is_none());

    // And it does not leave a tick lying around for the next prompt.
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: california(),
    });
    assert!(
        !prompt(&core).acknowledged,
        "an acknowledgement arriving before the prompt pre-ticked it"
    );
    assert!(!prompt(&core).start_enabled);
}

#[test]
fn acknowledging_is_not_a_way_to_start_a_recording() {
    // CON-01. Ticking a box is not pressing Start, in any phase.
    for meeting in [zoom(), california()] {
        let mut core = armed(meeting);
        let effects = core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
        assert!(!effects.contains(&ShellEffect::StartCapture));
        assert!(!core.capture_is_live());
        assert!(core.view().pill.is_none());
    }

    // Including during a live session started some other way.
    let mut core = ShellCore::new();
    core.handle(ShellInput::Start {
        at: Monotonic::ZERO,
        origin: StartOrigin::Menu,
    });
    let effects = core.handle(ShellInput::PromptAcknowledged { acknowledged: true });
    assert!(effects.is_empty());
    assert!(core.view().prompt.is_none());
}

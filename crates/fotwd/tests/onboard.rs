//! Onboarding: verify by doing, and distrust a success you cannot trust.
//!
//! There is **no public API to query the macOS system-audio grant**, and a
//! denial delivers silence indistinguishable from a quiet room
//! (docs/REQUIREMENTS.md 6.3). So onboarding cannot ask; it has to capture and
//! measure. Everything in this file is about interpreting that measurement,
//! and it runs with no audio device and no TCC database because the reading
//! and the environment are both injected.
//!
//! The subtlest requirement is the one about *passing*: on a developer's
//! machine the round trip succeeds for reasons that will not hold for a user —
//! an unbundled binary run from a terminal inherits the terminal's grant. A
//! pass under those conditions is not evidence, and saying so loudly is the
//! whole of issue #31's second half.

use std::time::Duration;

use fotwd::onboard::{
    CodeSignature, Environment, Outcome, ProbeReading, Report, Step, TrustWarning, interpret,
};

fn shipped() -> Environment {
    Environment {
        in_app_bundle: true,
        signature: CodeSignature::Authority {
            name: "Developer ID Application: Example (ABCDE12345)".to_owned(),
        },
        terminal: None,
    }
}

fn developer_shell() -> Environment {
    Environment {
        in_app_bundle: false,
        signature: CodeSignature::AdHoc,
        terminal: Some("ghostty".to_owned()),
    }
}

fn captured() -> ProbeReading {
    ProbeReading {
        started: true,
        callbacks: 140,
        samples: 288_000,
        nonzero: 190_000,
        tone_played: true,
        error: None,
    }
}

// --- the environment ----------------------------------------------------

#[test]
fn an_unbundled_binary_run_from_a_terminal_is_never_trustworthy_evidence() {
    // The single most expensive failure in this project: it works on the
    // developer's machine because the *terminal* holds the grant, and it
    // records silence for every user. Verified in testing — an ad-hoc-signed
    // binary captured real system audio with no prompt at all.
    let warnings = developer_shell().warnings();

    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, TrustWarning::InheritedTerminalGrant { .. })),
        "got {warnings:?}"
    );
    assert!(!developer_shell().evidence_is_trustworthy());

    // And it is the *first* thing said, because it invalidates everything
    // else on the screen.
    assert!(matches!(
        warnings.first(),
        Some(TrustWarning::InheritedTerminalGrant { .. })
    ));
    assert!(warnings[0].headline().to_lowercase().contains("terminal"));
}

#[test]
fn an_adhoc_signature_is_reported_as_an_identity_that_churns() {
    // TCC keys its record off the Designated Requirement. An ad-hoc signature
    // mints a new cdhash-based one on every rebuild, so the grant is dropped
    // every time the developer types `cargo build`.
    let env = Environment {
        in_app_bundle: true,
        signature: CodeSignature::AdHoc,
        terminal: None,
    };
    let warnings = env.warnings();
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, TrustWarning::IdentityChurnsEveryBuild)),
        "got {warnings:?}"
    );
    assert!(!env.evidence_is_trustworthy());
}

#[test]
fn an_unbundled_binary_is_flagged_even_when_it_was_not_run_from_a_terminal() {
    // No bundle means no Info.plist, which means no
    // NSAudioCaptureUsageDescription — and a missing usage description
    // suppresses the TCC prompt entirely rather than failing.
    let env = Environment {
        in_app_bundle: false,
        signature: CodeSignature::Authority {
            name: "Developer ID Application: Example (ABCDE12345)".to_owned(),
        },
        terminal: None,
    };
    assert!(
        env.warnings()
            .iter()
            .any(|w| matches!(w, TrustWarning::NotInBundle))
    );
}

#[test]
fn a_bundled_binary_launched_from_a_shell_is_still_the_terminals_grant() {
    // The trap the justfile warns about in as many words: running
    // `FlyOnTheWall.app/Contents/MacOS/fotwd` from a terminal makes the
    // *terminal* the responsible process, exactly as a bare binary does. The
    // bundle only helps when the bundle is what gets launched — `open -a`,
    // Finder, launchd. A trust rule that keyed off "is it in a bundle" would
    // hand a clean result to the one habit that is specifically called out.
    let env = Environment {
        in_app_bundle: true,
        signature: CodeSignature::SelfSigned {
            common_name: "FlyOnTheWall Dev".to_owned(),
        },
        terminal: Some("iTerm2".to_owned()),
    };
    assert!(
        env.warnings()
            .iter()
            .any(|w| matches!(w, TrustWarning::InheritedTerminalGrant { .. })),
        "a shell-launched bundle was treated as trustworthy: {:?}",
        env.warnings()
    );
    assert!(!env.evidence_is_trustworthy());
}

#[test]
fn a_signed_bundle_launched_normally_is_trustworthy() {
    // The one configuration whose result means anything for users.
    assert!(shipped().warnings().is_empty());
    assert!(shipped().evidence_is_trustworthy());
}

#[test]
fn the_dev_signing_identity_is_trustworthy_enough_to_report_a_real_result() {
    // `just dev-sign` mints a persisted self-signed identity precisely so the
    // Designated Requirement is stable across rebuilds. It is not a shipping
    // signature, but a capture under it is real evidence.
    let env = Environment {
        in_app_bundle: true,
        signature: CodeSignature::SelfSigned {
            common_name: "FlyOnTheWall Dev".to_owned(),
        },
        terminal: None,
    };
    assert!(env.warnings().is_empty(), "got {:?}", env.warnings());
}

#[test]
fn codesign_output_is_read_rather_than_guessed_at() {
    assert_eq!(
        CodeSignature::parse("test-exe: code object is not signed at all"),
        CodeSignature::Unsigned
    );
    assert_eq!(
        CodeSignature::parse(
            "Identifier=fotwd\nFormat=Mach-O thin (arm64)\nSignature=adhoc\nInfo.plist=not bound"
        ),
        CodeSignature::AdHoc
    );
    assert_eq!(
        CodeSignature::parse("Identifier=com.flyonthewall.fotw\nAuthority=FlyOnTheWall Dev\n"),
        CodeSignature::SelfSigned {
            common_name: "FlyOnTheWall Dev".to_owned()
        }
    );
    assert_eq!(
        CodeSignature::parse(
            "Identifier=com.flyonthewall.fotw\n\
             Authority=Developer ID Application: Example (ABCDE12345)\n\
             Authority=Developer ID Certification Authority\n\
             Authority=Apple Root CA\n"
        ),
        CodeSignature::Authority {
            name: "Developer ID Application: Example (ABCDE12345)".to_owned()
        }
    );
}

// --- interpreting the round trip ----------------------------------------

#[test]
fn a_capture_that_worked_in_an_untrustworthy_environment_does_not_read_as_a_pass() {
    // This is the assertion the whole file exists for. The samples arrived.
    // It still is not evidence that a *user* will get samples.
    let outcome = interpret(Step::SystemAudio, &captured(), &developer_shell());

    match outcome {
        Outcome::VerifiedButUnsound { warnings, .. } => {
            assert!(
                warnings
                    .iter()
                    .any(|w| matches!(w, TrustWarning::InheritedTerminalGrant { .. }))
            );
        }
        other => panic!("a developer-shell pass reported as {other:?}"),
    }
}

#[test]
fn the_same_capture_in_a_signed_bundle_is_a_pass() {
    assert!(matches!(
        interpret(Step::SystemAudio, &captured(), &shipped()),
        Outcome::Verified { .. }
    ));
}

#[test]
fn no_callbacks_at_all_is_a_failure_with_an_actionable_remedy() {
    let reading = ProbeReading {
        started: true,
        callbacks: 0,
        samples: 0,
        nonzero: 0,
        tone_played: true,
        error: None,
    };
    let Outcome::Failed { remedy, .. } = interpret(Step::SystemAudio, &reading, &shipped()) else {
        panic!("zero callbacks must fail");
    };
    assert!(remedy.settings_url.is_some());
    assert!(!remedy.commands.is_empty());
}

#[test]
fn silence_while_a_tone_is_playing_is_a_denial_and_says_so() {
    // The round trip's whole point: with a tone playing through the default
    // output, silence is no longer ambiguous.
    let reading = ProbeReading {
        started: true,
        callbacks: 140,
        samples: 288_000,
        nonzero: 0,
        tone_played: true,
        error: None,
    };
    let outcome = interpret(Step::SystemAudio, &reading, &shipped());
    let Outcome::Failed { detail, .. } = &outcome else {
        panic!("got {outcome:?}");
    };
    assert!(
        detail.to_lowercase().contains("denied"),
        "the user must be told this is a permission problem: {detail}"
    );
}

#[test]
fn silence_with_no_tone_playing_is_inconclusive_rather_than_a_denial() {
    // Telling a user their permission is denied when the truth is "nothing
    // was playing" sends them into System Settings to fix a working system.
    let reading = ProbeReading {
        started: true,
        callbacks: 140,
        samples: 288_000,
        nonzero: 0,
        tone_played: false,
        error: None,
    };
    assert!(matches!(
        interpret(Step::SystemAudio, &reading, &shipped()),
        Outcome::Inconclusive { .. }
    ));
}

#[test]
fn a_tap_that_could_not_start_reports_the_error_rather_than_a_permission_story() {
    let reading = ProbeReading {
        started: false,
        error: Some("AudioHardwareCreateProcessTap failed: -4".to_owned()),
        ..ProbeReading::default()
    };
    let Outcome::Failed { detail, .. } = interpret(Step::SystemAudio, &reading, &shipped()) else {
        panic!("a tap that never started must fail");
    };
    assert!(detail.contains("-4"), "{detail}");
}

#[test]
fn a_quiet_room_does_not_fail_the_microphone_step() {
    // Asymmetry on purpose. We can play a tone into the system tap; we cannot
    // make the user speak. For the mic leg, callbacks arriving is the pass
    // condition and all-zero samples is normal in a silent room.
    let reading = ProbeReading {
        started: true,
        callbacks: 100,
        samples: 48_000,
        nonzero: 0,
        tone_played: false,
        error: None,
    };
    assert!(matches!(
        interpret(Step::Microphone, &reading, &shipped()),
        Outcome::Verified { .. }
    ));

    // But no callbacks at all still fails: the device never ran.
    let dead = ProbeReading {
        callbacks: 0,
        ..reading
    };
    assert!(matches!(
        interpret(Step::Microphone, &dead, &shipped()),
        Outcome::Failed { .. }
    ));
}

// --- the remedies -------------------------------------------------------

#[test]
fn the_remedy_names_the_pane_the_grant_actually_lives_in() {
    // Since macOS 15 the system-audio grant is surfaced as "System Audio
    // Recording Only" inside Privacy & Security -> Screen & System Audio
    // Recording. Sending users to a "Microphone" pane they will not find it
    // in is the most common support failure for this class of app.
    let reading = ProbeReading {
        started: true,
        callbacks: 0,
        samples: 0,
        nonzero: 0,
        tone_played: true,
        error: None,
    };
    let Outcome::Failed { remedy, .. } = interpret(Step::SystemAudio, &reading, &shipped()) else {
        panic!()
    };
    let url = remedy.settings_url.expect("a deep link");
    assert!(url.contains("Privacy_ScreenCapture"), "{url}");
    assert!(
        remedy.headline.contains("Screen & System Audio Recording"),
        "{}",
        remedy.headline
    );
}

#[test]
fn the_remedy_cites_the_tcc_service_that_exists() {
    // `tccutil reset AudioCapture <bundle-id>`. Several widely-cited 2026
    // write-ups — and GitHub issue #31 itself — say the service is
    // `SystemAudioCaptureRequests`; that string does not exist, and a user who
    // runs it gets an error that reads as "this app is broken".
    let reading = ProbeReading {
        started: true,
        callbacks: 0,
        samples: 0,
        nonzero: 0,
        tone_played: true,
        error: None,
    };
    let Outcome::Failed { remedy, .. } = interpret(Step::SystemAudio, &reading, &shipped()) else {
        panic!()
    };
    let commands = remedy.commands.join("\n");
    assert!(
        commands.contains("tccutil reset AudioCapture"),
        "{commands}"
    );
    assert!(
        !commands.contains("SystemAudioCaptureRequests"),
        "cited a TCC service name that does not exist"
    );
}

// --- the report ---------------------------------------------------------

#[test]
fn a_report_is_only_ready_when_every_step_verified_soundly() {
    let mut report = Report::default();
    report.push(
        Step::SystemAudio,
        interpret(Step::SystemAudio, &captured(), &shipped()),
    );
    report.push(
        Step::Microphone,
        interpret(Step::Microphone, &captured(), &shipped()),
    );
    assert!(report.ready());
    assert!(report.render().contains("ready"));

    // A pass that cannot be trusted is not ready either: that is the entire
    // point of distinguishing the two.
    let mut report = Report::default();
    report.push(
        Step::SystemAudio,
        interpret(Step::SystemAudio, &captured(), &developer_shell()),
    );
    assert!(
        !report.ready(),
        "an inherited-grant pass was reported as a working install"
    );
    let rendered = report.render().to_lowercase();
    assert!(rendered.contains("terminal"), "{rendered}");
}

#[test]
fn the_report_never_claims_a_permission_it_only_inferred() {
    // No line of onboarding output may say the grant is present. The only
    // truthful statement is about what arrived: samples, or not.
    let mut report = Report::default();
    report.push(
        Step::SystemAudio,
        interpret(Step::SystemAudio, &captured(), &shipped()),
    );
    let rendered = report.render().to_lowercase();
    for claim in ["permission granted", "access granted", "authorized"] {
        assert!(!rendered.contains(claim), "onboarding claimed {claim:?}");
    }
}

#[test]
fn the_probe_window_is_long_enough_to_be_meaningful() {
    // A 100 ms probe on a busy machine can legitimately see zero callbacks,
    // which would report a denial to a user whose permissions are fine.
    assert!(fotwd::onboard::PROBE_WINDOW >= Duration::from_millis(750));
}

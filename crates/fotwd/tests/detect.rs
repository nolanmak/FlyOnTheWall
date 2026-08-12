//! Meeting detection: the conjunction, the dwell, and what must never happen.
//!
//! Every test here runs with **no conferencing app installed and no audio
//! device present** — the machine state is a `FixedActivityProbe` and the
//! clock is injected, which is the whole reason the probe sits behind a trait
//! (docs/REQUIREMENTS.md 5.6).
//!
//! Two requirements are on trial:
//!
//! - **MTG-03**: detection is *(a known conferencing app is running)* **and**
//!   *(the mic is hot)*, never one signal alone. The rationale is a consent
//!   argument, not a UX one: Zoom idles in the background all day, a naive
//!   detector prompts several times a day, and habituated dismissal destroys
//!   the all-party warning that rides on the same surface.
//! - **MTG-04 / CON-01**: detection arms; the user starts. The last test in
//!   this file drives the real `ShellCore` and asserts capture never begins.

use std::time::Duration;

use fotw_audio::activity::{ActivitySnapshot, AudioClient, InputDevice, Transport};
use fotw_audio::testing::FixedActivityProbe;
use fotw_shell::{Monotonic, ShellCore, ShellEffect, ShellInput};
use fotwd::detect::{
    CalendarEvent, Detection, Detector, DetectorConfig, FixedCalendar, NoCalendar,
};

const OUR_PID: u32 = 999;

fn detector() -> Detector {
    Detector::new(DetectorConfig {
        self_pid: OUR_PID,
        ..DetectorConfig::default()
    })
}

/// A machine with a built-in mic and whatever clients the test names.
fn machine(clients: Vec<AudioClient>) -> ActivitySnapshot {
    ActivitySnapshot {
        clients,
        default_input: Some(InputDevice::new(
            "MacBook Air Microphone",
            Transport::BuiltIn,
            false,
        )),
    }
}

/// A process on a *stock, idle* Mac that reports itself as holding the input.
/// Measured on macOS 26.3: `com.apple.CoreSpeech` did this with nothing
/// playing and no call in progress, and stopped doing it a few minutes later
/// with nothing having changed.
fn always_listening() -> AudioClient {
    AudioClient::new(762, Some("com.apple.CoreSpeech")).with_input(true)
}

fn zoom_in_a_call() -> AudioClient {
    AudioClient::new(101, Some("us.zoom.xos"))
        .with_input(true)
        .with_output(true)
}

fn zoom_idle() -> AudioClient {
    AudioClient::new(101, Some("us.zoom.xos"))
}

/// Poll until either the detector arms or `limit` polls elapse, one second
/// apart. Returns the detection and the second it happened.
fn poll_for(
    detector: &mut Detector,
    snapshot: &ActivitySnapshot,
    from: u64,
    limit: u64,
) -> Option<(u64, Detection)> {
    for sec in from..from + limit {
        let at = Monotonic::from_secs(sec);
        match detector.poll(at, Ok(snapshot), &NoCalendar) {
            Detection::Arm(m) => return Some((sec, Detection::Arm(m))),
            _ => continue,
        }
    }
    None
}

#[test]
fn an_idle_conferencing_app_never_arms() {
    // The single most important negative case. Zoom and Teams sit in the
    // background on most installs; arming on process presence alone is the
    // "prompts several times a day" failure that trains users to dismiss the
    // consent warning without reading it.
    let mut detector = detector();
    let snapshot = machine(vec![zoom_idle()]);

    assert!(
        poll_for(&mut detector, &snapshot, 0, 600).is_none(),
        "ten minutes of an idle Zoom armed the detector"
    );
}

#[test]
fn a_hot_mic_alone_never_arms() {
    // Measured, not hypothetical: Siri's listening path holds an input client
    // on an idle machine. A detector that read "someone holds the mic" as half
    // of the conjunction would be half-armed by a system daemon.
    let mut detector = detector();
    let snapshot = machine(vec![always_listening()]);

    assert!(poll_for(&mut detector, &snapshot, 0, 600).is_none());
}

#[test]
fn an_always_listening_daemon_does_not_complete_the_conjunction_for_an_idle_app() {
    // The two negatives above, together, are the realistic idle machine: Zoom
    // in the background *and* CoreSpeech holding the mic. The conjunction has
    // to be per-app, not "app present AND some process holds the mic".
    let mut detector = detector();
    let snapshot = machine(vec![zoom_idle(), always_listening()]);

    assert!(
        poll_for(&mut detector, &snapshot, 0, 600).is_none(),
        "an idle Zoom plus Siri's mic client armed the detector"
    );
}

#[test]
fn a_conferencing_app_holding_the_mic_arms_after_the_dwell() {
    let mut detector = detector();
    let dwell = DetectorConfig::default().dwell;
    let snapshot = machine(vec![zoom_in_a_call()]);

    // Nothing on the first poll: a one-frame blip is not a meeting.
    assert!(matches!(
        detector.poll(Monotonic::ZERO, Ok(&snapshot), &NoCalendar),
        Detection::Idle
    ));

    let (at, detection) =
        poll_for(&mut detector, &snapshot, 1, 600).expect("Zoom in a call must arm");
    assert!(
        Duration::from_secs(at) >= dwell,
        "armed after {at}s, before the {dwell:?} dwell had elapsed"
    );
    assert!(
        Duration::from_secs(at) <= dwell + Duration::from_secs(2),
        "armed at {at}s, far later than the dwell"
    );
    match detection {
        Detection::Arm(meeting) => {
            assert_eq!(meeting.app_key, "us.zoom.xos");
            assert_eq!(meeting.app_name, "Zoom");
            assert!(
                meeting.evidence.contains("microphone"),
                "the prompt must say why it fired: {}",
                meeting.evidence
            );
        }
        other => panic!("expected an arm, got {other:?}"),
    }
}

#[test]
fn arming_is_an_edge_and_does_not_repeat_every_poll() {
    // A prompt re-raised every second is the same habituation failure by a
    // different route.
    let mut detector = detector();
    let snapshot = machine(vec![zoom_in_a_call()]);
    let (armed_at, _) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");

    let mut arms = 0;
    for sec in armed_at + 1..armed_at + 600 {
        if let Detection::Arm(_) =
            detector.poll(Monotonic::from_secs(sec), Ok(&snapshot), &NoCalendar)
        {
            arms += 1;
        }
    }
    assert_eq!(arms, 0, "the detector re-armed {arms} times in ten minutes");
}

#[test]
fn the_call_ending_withdraws_the_prompt() {
    let mut detector = detector();
    let in_call = machine(vec![zoom_in_a_call()]);
    let (armed_at, _) = poll_for(&mut detector, &in_call, 0, 600).expect("armed");

    let quiet = machine(vec![zoom_idle()]);
    let cleared = (armed_at + 1..armed_at + 60).any(|sec| {
        matches!(
            detector.poll(Monotonic::from_secs(sec), Ok(&quiet), &NoCalendar),
            Detection::Clear
        )
    });
    assert!(
        cleared,
        "the prompt outlived the call it was offering to record"
    );
}

#[test]
fn a_probe_failure_withdraws_the_prompt_rather_than_holding_it() {
    // "The machine state is unknown" must not read as "the meeting is still
    // happening". Withdrawing costs a prompt; holding leaves an offer to
    // record a call that may have ended twenty minutes ago.
    let mut detector = detector();
    let snapshot = machine(vec![zoom_in_a_call()]);
    let (armed_at, _) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");

    let cleared = detector.poll(
        Monotonic::from_secs(armed_at + 1),
        Err("process list unavailable"),
        &NoCalendar,
    );
    assert!(matches!(cleared, Detection::Clear), "got {cleared:?}");
}

#[test]
fn never_for_this_app_stops_it_arming_at_all() {
    let mut detector = detector();
    let snapshot = machine(vec![zoom_in_a_call()]);
    let (armed_at, _) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");

    detector.suppress_app("us.zoom.xos");

    assert!(
        poll_for(&mut detector, &snapshot, armed_at + 1, 3_600).is_none(),
        "an hour after 'never for this app', it armed anyway"
    );
    assert!(detector.is_suppressed("us.zoom.xos"));
}

#[test]
fn suppressions_can_be_carried_across_a_restart() {
    // "Never for this app" that lasts until the daemon restarts is not a
    // preference, it is a bug. The Detector holds the set; persisting it is
    // the host's job, so the pair of methods that make that possible are
    // tested here rather than assumed.
    let mut before = detector();
    before.suppress_app("us.zoom.xos");
    let saved: Vec<String> = before.suppressions().map(ToOwned::to_owned).collect();
    assert_eq!(saved, vec!["us.zoom.xos".to_owned()]);

    let mut restarted = detector();
    restarted.restore_suppressions(saved);
    assert!(restarted.is_suppressed("us.zoom.xos"));
    let snapshot = machine(vec![zoom_in_a_call()]);
    assert!(
        poll_for(&mut restarted, &snapshot, 0, 600).is_none(),
        "the suppression did not survive the restart"
    );
}

#[test]
fn not_now_backs_off_for_the_snooze_window_and_then_asks_again() {
    let mut detector = detector();
    let snapshot = machine(vec![zoom_in_a_call()]);
    let (armed_at, _) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");

    let snooze = DetectorConfig::default().snooze;
    detector.snooze(Monotonic::from_secs(armed_at));

    // Silent for the whole snooze window...
    let during = poll_for(
        &mut detector,
        &snapshot,
        armed_at + 1,
        snooze.as_secs().saturating_sub(2),
    );
    assert!(during.is_none(), "re-armed during the snooze: {during:?}");

    // ...and then available again, because the meeting may still be running
    // and the user may have changed their mind.
    let after = poll_for(
        &mut detector,
        &snapshot,
        armed_at + snooze.as_secs() + 1,
        600,
    );
    assert!(after.is_some(), "the snooze never expired");
}

#[test]
fn our_own_recording_does_not_re_arm_the_detector() {
    // We hold the microphone while recording. Counting that as evidence would
    // make the detector fire off its own capture, forever.
    let mut detector = detector();
    let snapshot = machine(vec![
        zoom_idle(),
        AudioClient::new(OUR_PID, Some("com.flyonthewall.fotw"))
            .with_input(true)
            .with_output(true),
    ]);

    assert!(poll_for(&mut detector, &snapshot, 0, 600).is_none());
}

#[test]
fn a_client_carrying_our_own_pid_is_never_evidence() {
    // Belt and braces, and labelled as such: today the case above is already
    // covered by the catalog — `com.flyonthewall.fotw` is not a conferencing
    // app, so our own client can never satisfy the conjunction whatever it
    // does. `DetectorConfig::self_pid` is the second lock, and mutation
    // testing showed it was the *only* thing with no test of its own, so this
    // exercises it directly rather than leaving an unverified guard in a file
    // whose whole subject is not recording people by accident.
    let mut detector = detector();
    let snapshot = machine(vec![
        AudioClient::new(OUR_PID, Some("us.zoom.xos"))
            .with_input(true)
            .with_output(true),
    ]);

    assert!(
        poll_for(&mut detector, &snapshot, 0, 600).is_none(),
        "a client with our own pid was counted as a meeting"
    );
}

#[test]
fn a_browser_meeting_is_detected_through_the_helper_process() {
    // Google Meet renders audio from a Chrome *helper*, whose bundle id is
    // `com.google.Chrome.helper`. Matching only the main bundle id misses
    // every browser meeting, which is a large share of them.
    let mut detector = detector();
    let snapshot = machine(vec![
        AudioClient::new(1938, Some("com.google.Chrome")),
        AudioClient::new(2000, Some("com.google.Chrome.helper"))
            .with_input(true)
            .with_output(true),
    ]);

    let (_, detection) =
        poll_for(&mut detector, &snapshot, 0, 600).expect("a browser meeting arms");
    match detection {
        Detection::Arm(m) => assert_eq!(m.app_key, "com.google.Chrome"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_browser_playing_a_video_is_not_a_meeting() {
    // Chrome with output and no input is YouTube, not Meet. This is the
    // false positive that would fire on every user, every day.
    let mut detector = detector();
    let snapshot = machine(vec![
        AudioClient::new(1938, Some("com.google.Chrome")),
        AudioClient::new(2000, Some("com.google.Chrome.helper")).with_output(true),
    ]);

    assert!(poll_for(&mut detector, &snapshot, 0, 600).is_none());
}

#[test]
fn a_browser_recording_a_voice_message_is_not_a_meeting() {
    // A browser holding *only* the microphone is a dictation box, a voice
    // note, a speech-to-text demo. A call has audio going both ways, and that
    // is the only thing separating the two from here. Without this rule, every
    // "record a voice message" web page raises a consent prompt.
    let mut detector = detector();
    let snapshot = machine(vec![
        AudioClient::new(1938, Some("com.google.Chrome")),
        AudioClient::new(2000, Some("com.google.Chrome.helper")).with_input(true),
    ]);

    assert!(poll_for(&mut detector, &snapshot, 0, 600).is_none());
}

#[test]
fn a_dedicated_conferencing_app_needs_no_output_to_count() {
    // The mirror of the rule above, and the reason it is scoped to browsers:
    // everyone else on the call being muted is normal, and a Zoom call with
    // nobody talking is still a call.
    let mut detector = detector();
    let snapshot = machine(vec![
        AudioClient::new(101, Some("us.zoom.xos")).with_input(true),
    ]);

    assert!(poll_for(&mut detector, &snapshot, 0, 600).is_some());
}

#[test]
fn a_music_player_is_never_a_meeting_however_loud() {
    let mut detector = detector();
    let snapshot = machine(vec![
        AudioClient::new(828, Some("com.spotify.client")).with_output(true),
        always_listening(),
    ]);

    assert!(poll_for(&mut detector, &snapshot, 0, 600).is_none());
}

#[test]
fn a_bluetooth_input_falls_back_to_the_calendar_instead_of_guessing() {
    // Issue #22: a Bluetooth mic can report inactive while it is being
    // recorded, so mic-hot cannot be a *required* conjunct — but it also
    // cannot be dropped, or an idle Zoom arms. The documented fallback is
    // calendar-window plus app-running.
    let airpods = ActivitySnapshot {
        clients: vec![zoom_idle()],
        default_input: Some(InputDevice::new("AirPods Pro", Transport::Bluetooth, false)),
    };

    // Without a calendar event: no arm. One signal is never enough.
    let mut blind = detector();
    assert!(poll_for(&mut blind, &airpods, 0, 600).is_none());

    // With a matching event: arm, and say which signal it was.
    let mut with_calendar = detector();
    let calendar = FixedCalendar::new(CalendarEvent::new("Design review"));
    let mut armed = None;
    for sec in 0..600 {
        if let Detection::Arm(m) =
            with_calendar.poll(Monotonic::from_secs(sec), Ok(&airpods), &calendar)
        {
            armed = Some(m);
            break;
        }
    }
    let meeting = armed.expect("app running inside a calendar event must arm");
    assert_eq!(meeting.title.as_deref(), Some("Design review"));
    assert!(
        meeting.evidence.to_lowercase().contains("calendar"),
        "the prompt must say the calendar was the evidence: {}",
        meeting.evidence
    );
}

#[test]
fn the_calendar_alone_is_not_enough_either() {
    // A calendar event with no conferencing app running is a meeting someone
    // is attending from their phone, or a blocked-out hour.
    let mut detector = detector();
    let snapshot = machine(vec![always_listening()]);
    let calendar = FixedCalendar::new(CalendarEvent::new("Focus time"));

    for sec in 0..600 {
        assert!(
            !matches!(
                detector.poll(Monotonic::from_secs(sec), Ok(&snapshot), &calendar),
                Detection::Arm(_)
            ),
            "armed on a calendar event alone"
        );
    }
}

#[test]
fn the_conversion_counter_counts_locally_and_says_nothing_to_anyone() {
    // Issue #22 asks for a local-only prompt-to-start conversion counter, so
    // the detector can be tuned without telemetry. It is a plain in-process
    // counter; the test is here to keep it honest as the shape changes.
    let mut detector = detector();
    let snapshot = machine(vec![zoom_in_a_call()]);
    let (armed_at, _) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");

    assert_eq!(detector.stats().armed, 1);
    assert_eq!(detector.stats().started, 0);
    assert_eq!(detector.stats().conversion(), 0.0);

    detector.record_started();
    assert_eq!(detector.stats().started, 1);
    assert_eq!(detector.stats().conversion(), 1.0);

    detector.snooze(Monotonic::from_secs(armed_at));
    assert_eq!(detector.stats().snoozed, 1);
}

#[test]
fn the_prompt_carries_the_jurisdiction_warning_it_is_supposed_to() {
    // CON-05: the warning rides on the prompt, and an all-party jurisdiction
    // makes it blocking. Attaching it here rather than at the click is what
    // makes the user read it while deciding.
    let mut detector = Detector::new(DetectorConfig {
        self_pid: OUR_PID,
        home_jurisdiction: "US-CA".to_owned(),
        ..DetectorConfig::default()
    });
    let snapshot = machine(vec![zoom_in_a_call()]);
    let (_, detection) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");
    let Detection::Arm(meeting) = detection else {
        unreachable!()
    };

    assert!(
        meeting.requires_acknowledgement,
        "California is all-party: the prompt must block"
    );
    assert!(
        meeting.consent_notice.contains("§ 632"),
        "{}",
        meeting.consent_notice
    );
    assert!(meeting.consent_notice.contains("not legal advice"));

    // A one-party jurisdiction gets the reminder, not the modal.
    let mut detector = Detector::new(DetectorConfig {
        self_pid: OUR_PID,
        home_jurisdiction: "US-NY".to_owned(),
        ..DetectorConfig::default()
    });
    let (_, detection) = poll_for(&mut detector, &snapshot, 0, 600).expect("armed");
    let Detection::Arm(meeting) = detection else {
        unreachable!()
    };
    assert!(!meeting.requires_acknowledgement);
    assert!(!meeting.consent_notice.is_empty(), "still says something");
}

#[test]
fn the_detector_runs_off_the_probe_trait_with_no_audio_device_present() {
    // The shape `fotwd detect` actually uses: ask the platform for a snapshot,
    // hand the Result straight to the detector. Driving it through the trait
    // rather than around it is what proves the seam is the real input path and
    // not a parallel one the tests invented.
    use fotw_audio::activity::ActivityProbe;

    let probe = FixedActivityProbe::new(machine(vec![zoom_idle()]));
    let mut detector = detector();

    let poll = |detector: &mut Detector, sec: u64| {
        let snapshot = probe.snapshot();
        let reason;
        let arg = match &snapshot {
            Ok(s) => Ok(s),
            Err(e) => {
                reason = e.to_string();
                Err(reason.as_str())
            }
        };
        detector.poll(Monotonic::from_secs(sec), arg, &NoCalendar)
    };

    for sec in 0..60 {
        assert!(matches!(poll(&mut detector, sec), Detection::Idle));
    }

    probe.set(machine(vec![zoom_in_a_call()]));
    let armed = (60..200).any(|sec| matches!(poll(&mut detector, sec), Detection::Arm(_)));
    assert!(armed, "the call started and the detector never noticed");

    // And a probe that starts failing withdraws rather than holding.
    probe.fail("process list unavailable");
    assert!(matches!(poll(&mut detector, 300), Detection::Clear));
}

// --- the cross-crate CON-01 test ----------------------------------------

#[test]
fn a_detection_driven_into_the_real_shell_never_starts_a_recording() {
    // `fotw-shell` proves its own state machine cannot be made to record by
    // detection. This proves the *wiring*: the thing the daemon actually does
    // with a `Detection::Arm` is hand it to `ShellInput::MeetingDetected`,
    // and that path produces no `StartCapture` however long the meeting runs.
    let mut detector = detector();
    let mut shell = ShellCore::new();
    let snapshot = machine(vec![zoom_in_a_call()]);

    let mut arms = 0;
    for sec in 0..7_200 {
        let at = Monotonic::from_secs(sec);
        let effects = match detector.poll(at, Ok(&snapshot), &NoCalendar) {
            Detection::Arm(meeting) => {
                arms += 1;
                shell.handle(ShellInput::MeetingDetected { at, meeting })
            }
            Detection::Clear => shell.handle(ShellInput::DetectionCleared),
            Detection::Idle | Detection::Hold => Vec::new(),
        };
        assert!(
            !effects.contains(&ShellEffect::StartCapture),
            "detection started a recording at second {sec}"
        );
        assert!(!shell.capture_is_live());
    }

    assert_eq!(arms, 1, "a two-hour meeting must arm exactly once");
    assert!(
        shell.view().prompt.is_some(),
        "the affordance must still be there for the user to press"
    );
}

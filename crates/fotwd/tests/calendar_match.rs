//! #295: which calendar event, if any, names a recording.
//!
//! Every event here is invented. This repo is public; never paste a real
//! calendar entry into a fixture.

use fotwd::calendar::{Participation, ScheduledEvent, match_event};

const MIN: i64 = 60_000;
/// 10:00 on an arbitrary day, epoch ms. Only differences matter.
const T10: i64 = 1_800_000_000_000;

fn at(minutes_after_ten: i64) -> i64 {
    T10 + minutes_after_ten * MIN
}

fn event(uid: &str, title: &str, start: i64, end: i64) -> ScheduledEvent {
    ScheduledEvent {
        uid: uid.into(),
        title: title.into(),
        start_ms: start,
        end_ms: end,
        all_day: false,
        canceled: false,
        participation: Participation::Unknown,
    }
}

fn matched(events: &[ScheduledEvent], start: i64, end: i64) -> Option<&str> {
    match_event(events, start, end).map(|e| e.uid.as_str())
}

#[test]
fn single_overlap_matches() {
    let events = [event("a", "Weekly sync", at(0), at(30))];
    assert_eq!(matched(&events, at(2), at(27)), Some("a"));
}

#[test]
fn late_start_by_twelve_minutes_still_matches() {
    let events = [event("a", "Weekly sync", at(0), at(30))];
    assert_eq!(matched(&events, at(12), at(29)), Some("a"));
}

#[test]
fn overrun_by_twenty_five_minutes_still_matches() {
    let events = [event("a", "Weekly sync", at(0), at(30))];
    assert_eq!(matched(&events, at(0), at(55)), Some("a"));
}

#[test]
fn recording_that_begins_after_the_event_ends_but_inside_the_grace_matches() {
    // Window is [start - 10m, end + 30m]: 10:45 is inside 10:30 + 30m.
    let events = [event("a", "Weekly sync", at(0), at(30))];
    assert_eq!(matched(&events, at(45), at(50)), Some("a"));
}

#[test]
fn recording_that_ends_more_than_ten_minutes_before_the_event_does_not_match() {
    let events = [event("a", "Weekly sync", at(0), at(30))];
    assert_eq!(matched(&events, at(-25), at(-11)), None);
}

#[test]
fn recording_long_after_the_event_does_not_match() {
    let events = [event("a", "Weekly sync", at(0), at(30))];
    assert_eq!(matched(&events, at(61), at(80)), None);
}

#[test]
fn back_to_back_picks_the_event_with_more_real_overlap() {
    let events = [
        event("first", "Planning", at(0), at(30)),
        event("second", "Design review", at(30), at(60)),
    ];
    assert_eq!(matched(&events, at(28), at(58)), Some("second"));
}

#[test]
fn equal_overlap_breaks_on_the_nearest_start() {
    // Both cover the whole recording; "near" starts 2 minutes off, "far" 20.
    let events = [
        event("far", "Office hours", at(-20), at(60)),
        event("near", "Interview loop", at(-2), at(60)),
    ];
    assert_eq!(matched(&events, at(0), at(30)), Some("near"));
}

#[test]
fn equal_overlap_and_start_breaks_on_acceptance() {
    let mut accepted = event("accepted", "Retro", at(0), at(30));
    accepted.participation = Participation::Accepted;
    let tentative = ScheduledEvent {
        participation: Participation::Tentative,
        ..event("tentative", "Retro (copy)", at(0), at(30))
    };
    assert_eq!(
        matched(&[tentative.clone(), accepted.clone()], at(0), at(30)),
        Some("accepted")
    );
    assert_eq!(
        matched(&[accepted, tentative], at(0), at(30)),
        Some("accepted")
    );
}

#[test]
fn a_full_tie_is_decided_by_uid_so_the_answer_never_flips() {
    let x = event("b-uid", "Standup", at(0), at(15));
    let y = event("a-uid", "Standup", at(0), at(15));
    assert_eq!(
        matched(&[x.clone(), y.clone()], at(0), at(15)),
        Some("a-uid")
    );
    assert_eq!(matched(&[y, x], at(0), at(15)), Some("a-uid"));
}

#[test]
fn all_day_events_are_never_chosen() {
    let mut holiday = event("a", "Company offsite", at(-600), at(840));
    holiday.all_day = true;
    assert_eq!(matched(&[holiday], at(0), at(30)), None);
}

#[test]
fn declined_events_are_never_chosen() {
    let mut declined = event("a", "Optional talk", at(0), at(30));
    declined.participation = Participation::Declined;
    assert_eq!(matched(&[declined], at(0), at(30)), None);
}

#[test]
fn canceled_events_are_never_chosen() {
    let mut canceled = event("a", "Vendor demo", at(0), at(30));
    canceled.canceled = true;
    assert_eq!(matched(&[canceled], at(0), at(30)), None);
}

#[test]
fn untitled_events_are_never_chosen() {
    let events = [event("a", "   ", at(0), at(30))];
    assert_eq!(matched(&events, at(0), at(30)), None);
}

#[test]
fn an_ineligible_event_does_not_hide_an_eligible_one() {
    let mut all_day = event("all-day", "Focus day", at(-600), at(840));
    all_day.all_day = true;
    let events = [all_day, event("real", "Customer call", at(0), at(30))];
    assert_eq!(matched(&events, at(0), at(30)), Some("real"));
}

#[test]
fn an_event_with_its_end_before_its_start_is_ignored() {
    let events = [event("a", "Broken entry", at(30), at(0))];
    assert_eq!(matched(&events, at(0), at(30)), None);
}

#[test]
fn a_recording_with_no_events_matches_nothing() {
    assert_eq!(matched(&[], at(0), at(30)), None);
}

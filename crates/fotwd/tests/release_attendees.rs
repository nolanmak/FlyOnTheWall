//! #327: the one calendar event a release may go out from, and who it goes to.
//!
//! Every event, name and address here is invented (`example.com`,
//! `example.org`). This repo is public; never paste a real invite here.

use std::sync::Arc;
use std::time::Duration;

use fotwd::calendar::release::{
    MAX_RECIPIENTS, NotEligible, address_allowed, confident_match, lookup_confident,
    normalize_address, recipients,
};
use fotwd::calendar::{
    Attendee, AttendeeKind, CalendarLookup, Fixed, Many, Participation, Response, ScheduledEvent,
};

const MIN: i64 = 60_000;
/// 10:00 on an arbitrary day, epoch ms. Only differences matter.
const T10: i64 = 1_800_000_000_000;

fn at(minutes_after_ten: i64) -> i64 {
    T10 + minutes_after_ten * MIN
}

fn event(
    source: &'static str,
    uid: &str,
    ical: Option<&str>,
    start: i64,
    end: i64,
) -> ScheduledEvent {
    ScheduledEvent {
        uid: uid.into(),
        title: format!("Meeting {uid}"),
        start_ms: start,
        end_ms: end,
        all_day: false,
        canceled: false,
        participation: Participation::Accepted,
        source,
        invitees: Vec::new(),
        ical_uid: ical.map(str::to_owned),
        attendees: Vec::new(),
    }
}

fn person(address: &str, response: Response) -> Attendee {
    Attendee {
        address: Some(address.into()),
        display_name: None,
        response,
        kind: AttendeeKind::Person,
        is_self: false,
        is_organizer: false,
    }
}

fn domains(list: &[&str]) -> Vec<String> {
    list.iter().map(|d| (*d).to_owned()).collect()
}

fn chosen(
    events: &[ScheduledEvent],
    start: i64,
    end: i64,
    source: &str,
    uid: &str,
) -> Result<(String, i64), NotEligible> {
    confident_match(events, start, end, source, uid).map(|e| (e.uid.clone(), e.start_ms))
}

// --- Candidates and de-duplication --------------------------------------------

#[test]
fn the_same_invite_in_two_calendars_counts_as_one_candidate() {
    let events = [
        event(
            "eventkit",
            "ext-1",
            Some("invite-1@example.com"),
            at(0),
            at(30),
        ),
        event(
            "composio",
            "g-1",
            Some("invite-1@example.com"),
            at(0),
            at(30),
        ),
    ];
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
    assert_eq!(
        chosen(&events, at(0), at(30), "eventkit", "ext-1"),
        Ok(("ext-1".into(), at(0)))
    );
}

#[test]
fn two_different_invites_covering_the_recording_are_not_confident() {
    let events = [
        event(
            "composio",
            "g-1",
            Some("invite-1@example.com"),
            at(0),
            at(30),
        ),
        event(
            "composio",
            "g-2",
            Some("invite-2@example.com"),
            at(0),
            at(30),
        ),
    ];
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "g-1"),
        Err(NotEligible::NoConfidentMatch)
    );
    // Copies without an iCalendar UID cannot be shown to be one invite.
    let events = [
        event("eventkit", "a", None, at(0), at(30)),
        event("composio", "b", None, at(0), at(30)),
    ];
    assert_eq!(
        chosen(&events, at(0), at(30), "eventkit", "a"),
        Err(NotEligible::NoConfidentMatch)
    );
}

#[test]
fn a_long_focus_block_is_not_a_candidate() {
    let events = [
        event(
            "eventkit",
            "focus",
            Some("focus@example.com"),
            at(-60),
            at(420),
        ),
        event(
            "composio",
            "g-1",
            Some("invite-1@example.com"),
            at(0),
            at(30),
        ),
    ];
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
}

#[test]
fn a_declined_event_is_not_a_candidate() {
    let mut declined = event(
        "composio",
        "g-2",
        Some("invite-2@example.com"),
        at(0),
        at(30),
    );
    declined.participation = Participation::Declined;
    let events = [
        declined,
        event(
            "composio",
            "g-1",
            Some("invite-1@example.com"),
            at(0),
            at(30),
        ),
    ];
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
    // Nor can a declined event be the release's event.
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "g-2"),
        Err(NotEligible::NoConfidentMatch)
    );
}

// --- Finding the chosen copy ----------------------------------------------------

#[test]
fn calendar_uid_is_looked_up_within_its_own_source() {
    // The same identifier in another store is a different event.
    let events = [event(
        "eventkit",
        "id-1",
        Some("i1@example.com"),
        at(0),
        at(30),
    )];
    assert_eq!(
        chosen(&events, at(0), at(30), "eventkit", "id-1"),
        Ok(("id-1".into(), at(0)))
    );
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "id-1"),
        Err(NotEligible::EventNotFound)
    );
}

#[test]
fn a_recurring_event_resolves_to_the_instance_overlapping_the_recording() {
    // EventKit gives every occurrence the series' identifier.
    let day = 24 * 60;
    let events = [
        event(
            "eventkit",
            "series-1",
            Some("s1@example.com"),
            at(-day),
            at(-day + 30),
        ),
        event(
            "eventkit",
            "series-1",
            Some("s1@example.com"),
            at(0),
            at(30),
        ),
        event(
            "eventkit",
            "series-1",
            Some("s1@example.com"),
            at(day),
            at(day + 30),
        ),
    ];
    assert_eq!(
        chosen(&events, at(1), at(29), "eventkit", "series-1"),
        Ok(("series-1".into(), at(0)))
    );
    // Google gives each instance its own id under one iCalendar UID.
    let events = [
        event(
            "composio",
            "s_20270115T100000Z",
            Some("s1@example.com"),
            at(-day),
            at(-day + 30),
        ),
        event(
            "composio",
            "s_20270116T100000Z",
            Some("s1@example.com"),
            at(0),
            at(30),
        ),
    ];
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "s_20270116T100000Z"),
        Ok(("s_20270116T100000Z".into(), at(0)))
    );
}

#[test]
fn a_missing_event_is_event_not_found() {
    let events = [event(
        "composio",
        "g-1",
        Some("i1@example.com"),
        at(0),
        at(30),
    )];
    assert_eq!(
        chosen(&events, at(0), at(30), "composio", "gone"),
        Err(NotEligible::EventNotFound)
    );
    assert_eq!(
        chosen(&[], at(0), at(30), "composio", "g-1"),
        Err(NotEligible::EventNotFound)
    );
}

// --- The 50%-of-the-event rule ----------------------------------------------------

#[test]
fn a_meeting_running_to_twice_its_scheduled_length_still_passes() {
    let events = [event(
        "composio",
        "g-1",
        Some("i1@example.com"),
        at(0),
        at(30),
    )];
    assert_eq!(
        chosen(&events, at(0), at(60), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
}

#[test]
fn any_overlap_with_the_event_is_enough() {
    let events = [event(
        "composio",
        "g-1",
        Some("i1@example.com"),
        at(0),
        at(60),
    )];
    // Started seven minutes early and covered only the first few minutes.
    assert_eq!(
        chosen(&events, at(-7), at(5), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
    // A recording entirely outside the event is not this event.
    assert_eq!(
        chosen(&events, at(61), at(90), "composio", "g-1"),
        Err(NotEligible::EventNotFound)
    );
}

#[test]
fn another_invite_overlapping_more_is_a_rival() {
    let events = [
        event("composio", "g-1", Some("a@example.com"), at(0), at(30)),
        event("composio", "g-2", Some("b@example.com"), at(30), at(60)),
    ];
    // Recorded 25..55: mostly the second meeting, so the first is not it.
    assert_eq!(
        chosen(&events, at(25), at(55), "composio", "g-1"),
        Err(NotEligible::NoConfidentMatch)
    );
    // Recorded 0..35: mostly the first, which still matches.
    assert_eq!(
        chosen(&events, at(0), at(35), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
}

#[test]
fn a_neighbour_that_is_mostly_outside_the_recording_is_no_rival() {
    let events = [
        event("composio", "g-1", Some("i1@example.com"), at(0), at(30)),
        event("composio", "g-2", Some("i2@example.com"), at(25), at(55)),
    ];
    assert_eq!(
        chosen(&events, at(0), at(32), "composio", "g-1"),
        Ok(("g-1".into(), at(0)))
    );
}

// --- Looking up through a calendar --------------------------------------------------

struct Broken;
impl CalendarLookup for Broken {
    fn source(&self) -> &'static str {
        "broken"
    }
    fn events_overlapping(&self, _: i64, _: i64) -> Result<Vec<ScheduledEvent>, String> {
        Err("down".into())
    }
}

struct Slow;
impl CalendarLookup for Slow {
    fn source(&self) -> &'static str {
        "slow"
    }
    fn events_overlapping(&self, _: i64, _: i64) -> Result<Vec<ScheduledEvent>, String> {
        std::thread::sleep(Duration::from_secs(5));
        Ok(Vec::new())
    }
}

#[test]
fn lookup_finds_the_chosen_copy_through_a_calendar() {
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Fixed::new(
        "test",
        vec![event("", "evt-1", Some("i1@example.com"), at(0), at(30))],
    ));
    let found = lookup_confident(
        calendar,
        "test",
        "evt-1",
        at(0),
        at(30),
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(found.uid, "evt-1");
    assert_eq!(
        found.source, "test",
        "an unnamed copy takes its store's name"
    );
}

#[test]
fn a_store_that_fails_makes_the_calendar_unavailable() {
    // Naming carries on with the stores that answered; release must not.
    let good: Arc<dyn CalendarLookup> = Arc::new(Fixed::new(
        "eventkit",
        vec![event(
            "eventkit",
            "evt-1",
            Some("i1@example.com"),
            at(0),
            at(30),
        )],
    ));
    let many = Many::new(vec![good, Arc::new(Broken)]);
    assert_eq!(many.events_overlapping(at(0), at(30)).unwrap().len(), 1);
    let found = lookup_confident(
        Arc::new(many),
        "eventkit",
        "evt-1",
        at(0),
        at(30),
        Duration::from_secs(2),
    );
    assert_eq!(found, Err(NotEligible::CalendarUnavailable));
}

#[test]
fn a_calendar_that_times_out_is_unavailable() {
    let found = lookup_confident(
        Arc::new(Slow),
        "slow",
        "evt-1",
        at(0),
        at(30),
        Duration::from_millis(50),
    );
    assert_eq!(found, Err(NotEligible::CalendarUnavailable));
}

#[test]
fn reason_codes_are_the_spec_codes() {
    assert_eq!(NotEligible::NoConfidentMatch.code(), "no_confident_match");
    assert_eq!(NotEligible::EventNotFound.code(), "event_not_found");
    assert_eq!(
        NotEligible::CalendarUnavailable.code(),
        "calendar_unavailable"
    );
}

// --- Recipients -------------------------------------------------------------------

fn with_attendees(attendees: Vec<Attendee>) -> ScheduledEvent {
    ScheduledEvent {
        attendees,
        ..event("composio", "g-1", Some("i1@example.com"), at(0), at(30))
    }
}

fn addresses(event: &ScheduledEvent, allowed: &[&str]) -> Vec<String> {
    recipients(event, &domains(allowed))
        .list
        .into_iter()
        .map(|r| r.address)
        .collect()
}

#[test]
fn groups_resources_and_decliners_are_excluded_and_the_organiser_kept() {
    let mut me = person("me@example.com", Response::Accepted);
    me.is_self = true;
    let mut host = person("host@example.com", Response::Unknown);
    host.is_organizer = true;
    let mut group = person("team@example.com", Response::NeedsAction);
    group.kind = AttendeeKind::Group;
    let mut room = person("room-2@example.com", Response::Accepted);
    room.kind = AttendeeKind::Resource;
    let event = with_attendees(vec![
        me,
        person("peer@example.com", Response::NeedsAction),
        person("maybe@example.com", Response::Tentative),
        person("no@example.com", Response::Declined),
        group,
        room,
        person("guest@example.org", Response::Accepted),
        host,
    ]);
    assert_eq!(
        addresses(&event, &["example.com"]),
        [
            "me@example.com",
            "peer@example.com",
            "maybe@example.com",
            "host@example.com"
        ]
    );
    let list = recipients(&event, &domains(&["example.com"])).list;
    assert!(list[0].is_self);
    assert!(list.iter().skip(1).all(|r| !r.is_self));
    assert_eq!(list[1].response, Response::NeedsAction);
}

#[test]
fn the_epic_acceptance_event_gives_exactly_two_recipients() {
    let mut me = person("me@example.com", Response::Accepted);
    me.is_self = true;
    let mut room = person("room-2@example.com", Response::Accepted);
    room.kind = AttendeeKind::Resource;
    let event = with_attendees(vec![
        me,
        person("colleague@example.com", Response::Accepted),
        person("declined@example.com", Response::Declined),
        person("guest@example.org", Response::Accepted),
        room,
    ]);
    assert_eq!(
        addresses(&event, &["example.com"]),
        ["me@example.com", "colleague@example.com"]
    );
}

#[test]
fn addresses_are_lowercased_and_subdomains_rejected() {
    let event = with_attendees(vec![
        person("Peer.One@EXAMPLE.com", Response::Accepted),
        person("peer@sub.example.com", Response::Accepted),
        person("peer@example.com.example.org", Response::Accepted),
        person("peer@notexample.com", Response::Accepted),
    ]);
    assert_eq!(
        addresses(&event, &["example.com"]),
        ["peer.one@example.com"]
    );
    assert!(!address_allowed(
        "peer@sub.example.com",
        &domains(&["example.com"])
    ));
    assert!(address_allowed(
        "peer@sub.example.com",
        &domains(&["sub.example.com"])
    ));
}

#[test]
fn a_person_listed_twice_is_one_recipient_and_any_decline_wins() {
    let mut host = person("HOST@example.com", Response::Unknown);
    host.is_organizer = true;
    let event = with_attendees(vec![
        person("peer@example.com", Response::Accepted),
        person("Peer@example.com", Response::Accepted),
        person("host@example.com", Response::Declined),
        host,
    ]);
    assert_eq!(addresses(&event, &["example.com"]), ["peer@example.com"]);
}

#[test]
fn attendees_without_a_plain_address_are_skipped() {
    let mut nameless = person("x", Response::Accepted);
    nameless.address = None;
    let event = with_attendees(vec![
        nameless,
        person("two@at@example.com", Response::Accepted),
        person("a,b@example.com", Response::Accepted),
        person("Display <c@example.com>", Response::Accepted),
        person("@example.com", Response::Accepted),
        person(" spaced@example.com ", Response::Accepted),
    ]);
    assert_eq!(addresses(&event, &["example.com"]), ["spaced@example.com"]);
    assert_eq!(normalize_address("a b@example.com"), None);
    assert_eq!(
        normalize_address(" Mixed@Example.COM "),
        Some("mixed@example.com".into())
    );
}

#[test]
fn no_allowed_domains_means_nobody() {
    let event = with_attendees(vec![person("peer@example.com", Response::Accepted)]);
    assert!(addresses(&event, &[]).is_empty());
}

#[test]
fn more_than_fifty_recipients_is_flagged_not_cut() {
    let people: Vec<Attendee> = (0..=MAX_RECIPIENTS)
        .map(|i| person(&format!("p{i}@example.com"), Response::Accepted))
        .collect();
    let over = recipients(&with_attendees(people.clone()), &domains(&["example.com"]));
    assert_eq!(over.list.len(), MAX_RECIPIENTS + 1);
    assert!(over.many_recipients);
    let at_cap = recipients(
        &with_attendees(people[..MAX_RECIPIENTS].to_vec()),
        &domains(&["example.com"]),
    );
    assert_eq!(at_cap.list.len(), MAX_RECIPIENTS);
    assert!(!at_cap.many_recipients);
}

#[test]
fn recipients_serialise_as_the_frozen_row_shape() {
    let mut me = person("me@example.com", Response::NeedsAction);
    me.is_self = true;
    me.display_name = Some("Me".into());
    let list = recipients(&with_attendees(vec![me]), &domains(&["example.com"])).list;
    assert_eq!(
        serde_json::to_value(&list).unwrap(),
        serde_json::json!([{
            "address": "me@example.com",
            "display_name": "Me",
            "response": "needs_action",
            "is_self": true,
        }])
    );
}

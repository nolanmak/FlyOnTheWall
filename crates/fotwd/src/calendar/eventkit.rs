//! The macOS calendar, read through EventKit (#295, the read slice of MTG-01).
//!
//! Read-only, and only what naming and attendee release need: title, times,
//! all-day, status, the user's own response, other attendees' names (not
//! rooms or resources) and, for release, each attendee's `mailto:` address,
//! response and type, the organiser, and the cross-device identifier. Notes,
//! location and links are never read.
//!
//! # Access
//!
//! Access is checked on every lookup and never requested there: a lookup runs
//! while a meeting is being saved, and a system dialog must not appear then.
//! [`request_access`] is what asks, and the daemon calls it once at startup
//! when the user has not answered yet. The grant belongs to the signed `.app`,
//! like the audio grant: test it from the bundle, not the bare binary.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::autoreleasepool;
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAuthorizationStatus, EKEntityType, EKEvent, EKEventStatus, EKEventStore, EKParticipant,
    EKParticipantStatus, EKParticipantType,
};
use objc2_foundation::{NSDate, NSError};

use super::{Attendee, AttendeeKind, CalendarLookup, Participation, Response, ScheduledEvent};

/// The user's calendar, through EventKit.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventKitCalendar;

/// Where calendar access stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Full access: lookups work.
    Granted,
    /// The user has not been asked yet.
    NotAsked,
    /// Denied, restricted, or write-only. Lookups return nothing.
    Refused,
}

impl Access {
    /// A short word for `fotwd doctor` and the log.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::NotAsked => "not asked yet",
            Self::Refused => "off (System Settings › Privacy & Security › Calendars)",
        }
    }
}

/// Current calendar access, without asking.
#[must_use]
pub fn access() -> Access {
    // SAFETY: a class method with a plain enum argument and result.
    let status = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
    if status == EKAuthorizationStatus::FullAccess {
        Access::Granted
    } else if status == EKAuthorizationStatus::NotDetermined {
        Access::NotAsked
    } else {
        Access::Refused
    }
}

/// Ask for full calendar access, waiting up to `timeout` for an answer.
///
/// Shows the system dialog only when the user has never answered; otherwise
/// it returns the standing answer at once.
#[must_use]
pub fn request_access(timeout: Duration) -> Access {
    if access() != Access::NotAsked {
        return access();
    }
    let (tx, rx) = mpsc::sync_channel(1);
    // A pool, because this runs on a plain Rust thread: Cocoa objects
    // autoreleased here would otherwise never be freed.
    autoreleasepool(|_| {
        let block = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            let _ = tx.try_send(granted.as_bool());
        });
        // SAFETY: `block` outlives the call — EventKit copies it — and the store is
        // kept alive until the answer arrives or we stop waiting.
        let store = unsafe { EKEventStore::new() };
        unsafe { store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&block)) };
    });
    match rx.recv_timeout(timeout) {
        Ok(true) => Access::Granted,
        Ok(false) => Access::Refused,
        Err(_) => access(),
    }
}

impl CalendarLookup for EventKitCalendar {
    fn source(&self) -> &'static str {
        "eventkit"
    }

    fn events_overlapping(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<ScheduledEvent>, String> {
        if access() != Access::Granted {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_precision_loss)]
        let (start, end) = (start_ms as f64 / 1000.0, end_ms as f64 / 1000.0);
        // SAFETY: plain EventKit reads on objects this function owns. The
        // predicate comes from the same store it is evaluated by, which is the
        // one precondition `eventsMatchingPredicate` documents.
        // Inside a pool: lookups run on a plain Rust thread, where Cocoa's
        // autoreleased objects would otherwise never be freed.
        Ok(autoreleasepool(|_| {
            let events = unsafe {
                let store = EKEventStore::new();
                let predicate = store.predicateForEventsWithStartDate_endDate_calendars(
                    &NSDate::dateWithTimeIntervalSince1970(start),
                    &NSDate::dateWithTimeIntervalSince1970(end),
                    None,
                );
                store.eventsMatchingPredicate(&predicate)
            };
            events.iter().map(|e| from_raw(&raw(&e))).collect()
        }))
    }
}

/// The fields of one `EKEvent` that naming uses, as plain values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawEvent {
    /// `calendarItemExternalIdentifier`: stable across devices.
    pub external_id: Option<String>,
    /// `eventIdentifier`.
    pub event_id: Option<String>,
    /// `calendarItemIdentifier`: always present, local to this device.
    pub item_id: String,
    /// The title.
    pub title: String,
    /// Start, seconds since 1970.
    pub start_s: f64,
    /// End, seconds since 1970.
    pub end_s: f64,
    /// `isAllDay`.
    pub all_day: bool,
    /// `status` is `EKEventStatusCanceled`.
    pub canceled: bool,
    /// The current user's attendee status, when they are an attendee.
    pub my_status: Option<Participation>,
    /// Other attendees' names, as the calendar shows them.
    pub invitees: Vec<String>,
    /// Every attendee, the user included, for attendee release.
    pub attendees: Vec<RawAttendee>,
    /// `event.organizer`, which EventKit may leave out of `attendees`.
    pub organizer: Option<RawAttendee>,
}

/// The fields of one `EKParticipant` that attendee release uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAttendee {
    /// The participant `URL`, as a string. Only `mailto:` gives an address.
    pub url: Option<String>,
    /// The participant `name`.
    pub name: Option<String>,
    /// `participantStatus`, mapped by [`response_of`].
    pub response: Response,
    /// `participantType`, mapped by [`kind_of`].
    pub kind: AttendeeKind,
    /// `isCurrentUser`.
    pub is_current_user: bool,
}

/// An `EKParticipantStatus` as a [`Response`].
#[must_use]
pub fn response_of(status: EKParticipantStatus) -> Response {
    if status == EKParticipantStatus::Accepted {
        Response::Accepted
    } else if status == EKParticipantStatus::Declined {
        Response::Declined
    } else if status == EKParticipantStatus::Tentative {
        Response::Tentative
    } else if status == EKParticipantStatus::Pending {
        Response::NeedsAction
    } else {
        // Unknown, delegated, completed, in process.
        Response::Unknown
    }
}

/// An `EKParticipantType` as an [`AttendeeKind`].
#[must_use]
pub fn kind_of(kind: EKParticipantType) -> AttendeeKind {
    if kind == EKParticipantType::Room || kind == EKParticipantType::Resource {
        AttendeeKind::Resource
    } else if kind == EKParticipantType::Group {
        AttendeeKind::Group
    } else {
        AttendeeKind::Person
    }
}

/// One `EKParticipant` as plain values.
///
/// # Safety
/// `p` must be a live participant of an event the store just returned.
unsafe fn raw_attendee(p: &EKParticipant) -> RawAttendee {
    // SAFETY: property reads on a live participant, as the caller promises.
    unsafe {
        RawAttendee {
            url: p.URL().absoluteString().map(|s| s.to_string()),
            name: p.name().map(|n| n.to_string()),
            response: response_of(p.participantStatus()),
            kind: kind_of(p.participantType()),
            is_current_user: p.isCurrentUser(),
        }
    }
}

fn raw(event: &EKEvent) -> RawEvent {
    // SAFETY: property reads on a live event the store just returned.
    unsafe {
        let attendees = event
            .attendees()
            .map(|people| people.iter().map(|p| raw_attendee(&p)).collect())
            .unwrap_or_default();
        let organizer = event.organizer().map(|p| raw_attendee(&p));
        let my_status = event.attendees().and_then(|people| {
            people.iter().find(|p| p.isCurrentUser()).map(|me| {
                let status = me.participantStatus();
                if status == EKParticipantStatus::Accepted {
                    Participation::Accepted
                } else if status == EKParticipantStatus::Declined {
                    Participation::Declined
                } else if status == EKParticipantStatus::Tentative {
                    Participation::Tentative
                } else {
                    Participation::Unknown
                }
            })
        });
        // Other people by name; rooms and resources are not people.
        let invitees = event
            .attendees()
            .map(|people| {
                people
                    .iter()
                    .filter(|p| !p.isCurrentUser())
                    .filter(|p| {
                        let kind = p.participantType();
                        kind != EKParticipantType::Room && kind != EKParticipantType::Resource
                    })
                    .filter_map(|p| p.name().map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        RawEvent {
            external_id: event
                .calendarItemExternalIdentifier()
                .map(|s| s.to_string()),
            event_id: event.eventIdentifier().map(|s| s.to_string()),
            item_id: event.calendarItemIdentifier().to_string(),
            title: event.title().to_string(),
            start_s: event.startDate().timeIntervalSince1970(),
            end_s: event.endDate().timeIntervalSince1970(),
            all_day: event.isAllDay(),
            canceled: event.status() == EKEventStatus::Canceled,
            my_status,
            invitees,
            attendees,
            organizer,
        }
    }
}

/// One `EKEvent`'s fields as a [`ScheduledEvent`].
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn from_raw(raw: &RawEvent) -> ScheduledEvent {
    let uid = [&raw.external_id, &raw.event_id]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| raw.item_id.clone());
    ScheduledEvent {
        uid,
        title: raw.title.clone(),
        start_ms: (raw.start_s * 1000.0).round() as i64,
        end_ms: (raw.end_s * 1000.0).round() as i64,
        all_day: raw.all_day,
        canceled: raw.canceled,
        participation: raw.my_status.unwrap_or(Participation::Unknown),
        source: "eventkit",
        invitees: raw
            .invitees
            .iter()
            .filter_map(|n| crate::invitees::invitee_name(Some(n), None))
            .collect(),
        ical_uid: raw.external_id.clone().filter(|s| !s.is_empty()),
        attendees: attendees_of(raw),
    }
}

/// The address in a participant URL: `mailto:` only, any `?query` dropped.
/// Anything else (`tel:`, `https:`, directory URLs) has no address.
fn mailto_address(url: &str) -> Option<String> {
    let scheme = url.get(..7)?;
    if !scheme.eq_ignore_ascii_case("mailto:") {
        return None;
    }
    let rest = &url[7..];
    let address = rest.split('?').next().unwrap_or_default().trim();
    (!address.is_empty()).then(|| address.to_owned())
}

/// Every attendee for attendee release, then `event.organizer` when the
/// attendee list leaves them out.
fn attendees_of(raw: &RawEvent) -> Vec<Attendee> {
    let convert = |p: &RawAttendee, is_organizer: bool| Attendee {
        address: p.url.as_deref().and_then(mailto_address),
        display_name: p
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        response: p.response,
        kind: p.kind,
        is_self: p.is_current_user,
        is_organizer,
    };
    let mut people: Vec<Attendee> = raw.attendees.iter().map(|p| convert(p, false)).collect();
    if let Some(organizer) = &raw.organizer {
        let host = convert(organizer, true);
        let same = |a: &Attendee| match (&a.address, &host.address) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        if let Some(listed) = people.iter_mut().find(|a| same(a)) {
            listed.is_organizer = true;
        } else if host.address.is_some() {
            people.push(host);
        }
    }
    people
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_event() -> RawEvent {
        RawEvent {
            external_id: Some("ext-1".into()),
            event_id: Some("evt-1".into()),
            item_id: "item-1".into(),
            title: "Design review".into(),
            start_s: 1_800_000_000.0,
            end_s: 1_800_001_800.5,
            all_day: false,
            canceled: false,
            my_status: Some(Participation::Accepted),
            invitees: vec!["Rafiya Lindqvist".into(), "Room <B2>".into()],
            attendees: Vec::new(),
            organizer: None,
        }
    }

    #[test]
    fn maps_times_to_epoch_milliseconds() {
        let e = from_raw(&raw_event());
        assert_eq!(e.start_ms, 1_800_000_000_000);
        assert_eq!(
            e.invitees,
            ["Rafiya Lindqvist"],
            "names kept, markup dropped"
        );
        assert_eq!(e.end_ms, 1_800_001_800_500);
        assert_eq!(e.title, "Design review");
        assert_eq!(e.participation, Participation::Accepted);
    }

    #[test]
    fn prefers_the_cross_device_identifier() {
        assert_eq!(from_raw(&raw_event()).uid, "ext-1");
        let no_ext = RawEvent {
            external_id: Some(String::new()),
            ..raw_event()
        };
        assert_eq!(from_raw(&no_ext).uid, "evt-1");
        let only_item = RawEvent {
            external_id: None,
            event_id: None,
            ..raw_event()
        };
        assert_eq!(from_raw(&only_item).uid, "item-1");
    }

    #[test]
    fn an_event_without_attendees_has_unknown_participation() {
        let mine = RawEvent {
            my_status: None,
            ..raw_event()
        };
        assert_eq!(from_raw(&mine).participation, Participation::Unknown);
    }

    #[test]
    fn carries_all_day_and_canceled() {
        let e = from_raw(&RawEvent {
            all_day: true,
            canceled: true,
            ..raw_event()
        });
        assert!(e.all_day && e.canceled);
    }

    fn person(url: &str, response: Response) -> RawAttendee {
        RawAttendee {
            url: Some(url.into()),
            name: Some("Teammate".into()),
            response,
            kind: AttendeeKind::Person,
            is_current_user: false,
        }
    }

    #[test]
    fn carries_the_ical_uid_from_the_external_identifier() {
        assert_eq!(from_raw(&raw_event()).ical_uid.as_deref(), Some("ext-1"));
        let none = RawEvent {
            external_id: Some(String::new()),
            ..raw_event()
        };
        assert_eq!(from_raw(&none).ical_uid, None, "empty is no UID");
    }

    #[test]
    fn attendee_fields_come_from_mailto_urls_only() {
        let mut me = person("mailto:Me@Example.com", Response::Accepted);
        me.is_current_user = true;
        let mut room = person("mailto:room-2@example.com", Response::Accepted);
        room.kind = AttendeeKind::Resource;
        let e = from_raw(&RawEvent {
            attendees: vec![
                me,
                person("tel:+15550100", Response::Accepted),
                person("https://example.com/people/1", Response::Tentative),
                person("MAILTO:peer@example.com", Response::Declined),
                room,
            ],
            ..raw_event()
        });
        let addresses: Vec<Option<&str>> =
            e.attendees.iter().map(|a| a.address.as_deref()).collect();
        assert_eq!(
            addresses,
            [
                Some("Me@Example.com"),
                None,
                None,
                Some("peer@example.com"),
                Some("room-2@example.com"),
            ]
        );
        assert!(e.attendees[0].is_self && !e.attendees[3].is_self);
        assert_eq!(e.attendees[3].response, Response::Declined);
        assert_eq!(e.attendees[4].kind, AttendeeKind::Resource);
        assert_eq!(e.attendees[0].display_name.as_deref(), Some("Teammate"));
    }

    #[test]
    fn the_organizer_is_added_when_absent_and_marked_when_present() {
        let organizer = person("mailto:host@example.com", Response::Unknown);
        let absent = from_raw(&RawEvent {
            attendees: vec![person("mailto:peer@example.com", Response::Accepted)],
            organizer: Some(organizer.clone()),
            ..raw_event()
        });
        assert_eq!(absent.attendees.len(), 2);
        let host = &absent.attendees[1];
        assert_eq!(host.address.as_deref(), Some("host@example.com"));
        assert!(host.is_organizer);
        assert!(!absent.attendees[0].is_organizer);

        let present = from_raw(&RawEvent {
            attendees: vec![person("mailto:HOST@example.com", Response::Accepted)],
            organizer: Some(organizer),
            ..raw_event()
        });
        assert_eq!(present.attendees.len(), 1, "not added twice");
        assert!(present.attendees[0].is_organizer);
        assert_eq!(present.attendees[0].response, Response::Accepted);
    }

    #[test]
    fn participant_status_and_type_map_to_release_values() {
        assert_eq!(
            response_of(EKParticipantStatus::Accepted),
            Response::Accepted
        );
        assert_eq!(
            response_of(EKParticipantStatus::Declined),
            Response::Declined
        );
        assert_eq!(
            response_of(EKParticipantStatus::Tentative),
            Response::Tentative
        );
        assert_eq!(
            response_of(EKParticipantStatus::Pending),
            Response::NeedsAction
        );
        assert_eq!(response_of(EKParticipantStatus::Unknown), Response::Unknown);
        assert_eq!(kind_of(EKParticipantType::Person), AttendeeKind::Person);
        assert_eq!(kind_of(EKParticipantType::Unknown), AttendeeKind::Person);
        assert_eq!(kind_of(EKParticipantType::Group), AttendeeKind::Group);
        assert_eq!(kind_of(EKParticipantType::Room), AttendeeKind::Resource);
        assert_eq!(kind_of(EKParticipantType::Resource), AttendeeKind::Resource);
    }

    /// Reads the real calendar. Opt-in, because it needs calendar access for
    /// whatever is running the tests, and CI must never prompt.
    #[test]
    fn live_lookup_does_not_fail() {
        if std::env::var_os("FOTW_EVENTKIT_TESTS").is_none() {
            return;
        }
        let now = fotw_store::now_ms();
        let events = EventKitCalendar
            .events_overlapping(now - 86_400_000, now + 86_400_000)
            .unwrap();
        eprintln!("access {:?}, {} events today", access(), events.len());
    }
}

//! The macOS calendar, read through EventKit (#295, the read slice of MTG-01).
//!
//! Read-only, and only what naming needs: title, times, all-day, status and the
//! user's own response. Attendee names, notes, location and links are never
//! read.
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
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAuthorizationStatus, EKEntityType, EKEvent, EKEventStatus, EKEventStore, EKParticipantStatus,
};
use objc2_foundation::{NSDate, NSError};

use super::{CalendarLookup, Participation, ScheduledEvent};

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
    let block = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        let _ = tx.try_send(granted.as_bool());
    });
    // SAFETY: `block` outlives the call — EventKit copies it — and the store is
    // kept alive until the answer arrives or we stop waiting.
    let store = unsafe { EKEventStore::new() };
    unsafe { store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&block)) };
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
        let events = unsafe {
            let store = EKEventStore::new();
            let predicate = store.predicateForEventsWithStartDate_endDate_calendars(
                &NSDate::dateWithTimeIntervalSince1970(start),
                &NSDate::dateWithTimeIntervalSince1970(end),
                None,
            );
            store.eventsMatchingPredicate(&predicate)
        };
        Ok(events.iter().map(|e| from_raw(&raw(&e))).collect())
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
}

fn raw(event: &EKEvent) -> RawEvent {
    // SAFETY: property reads on a live event the store just returned.
    unsafe {
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
    }
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
        }
    }

    #[test]
    fn maps_times_to_epoch_milliseconds() {
        let e = from_raw(&raw_event());
        assert_eq!(e.start_ms, 1_800_000_000_000);
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

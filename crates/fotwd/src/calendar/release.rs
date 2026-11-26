//! Who an attendee release goes to (#326 D3, D4; #327).
//!
//! Two pure steps on top of the naming matcher, never a second matcher:
//!
//! 1. [`confident_match`]: the copy the matcher chose for the meeting
//!    (`meetings.calendar_source` + `meetings.calendar_uid`), and only if it is
//!    the one event the recording clearly was.
//! 2. [`recipients`]: that copy's attendees, cut down to people in an allowed
//!    domain.
//!
//! Nothing here borrows from neighbouring events: no `pooled_invitees`, no
//! twin-event names. Those trade accuracy for recall, which is right for
//! fixing a spelling and wrong for choosing who gets an email.
//!
//! Errors and reasons are codes only. Nothing here logs.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    Attendee, AttendeeKind, CalendarLookup, EARLY_GRACE_MS, LATE_GRACE_MS, Response, ScheduledEvent,
};

/// Above this many recipients a release always needs a person's review.
pub const MAX_RECIPIENTS: usize = 50;

/// Why a meeting cannot be released from its calendar event (D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NotEligible {
    /// No single event clearly is the recording, or it is not the one the
    /// meeting was named after.
    #[error("no_confident_match")]
    NoConfidentMatch,
    /// The event the meeting was named after is no longer in its calendar
    /// for the recording's window.
    #[error("event_not_found")]
    EventNotFound,
    /// A calendar could not be read in full. Worth trying again later.
    #[error("calendar_unavailable")]
    CalendarUnavailable,
}

impl NotEligible {
    /// The reason code stored and shown for the meeting.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoConfidentMatch => "no_confident_match",
            Self::EventNotFound => "event_not_found",
            Self::CalendarUnavailable => "calendar_unavailable",
        }
    }
}

/// The copy of an event `meetings.calendar_uid` points at, if `events` show it
/// to be the one event the recording `[start_ms, end_ms]` clearly was.
///
/// `source` and `uid` are the meeting's stored `calendar_source` and
/// `calendar_uid`: a calendar store's own identifier, not an iCalendar UID.
/// A recurring event resolves to the instance that overlaps the recording.
///
/// Candidates are the events the matcher would consider: eligible, and not a
/// block many times the recording's length. Copies of one invite (the same
/// iCalendar UID and start, as Apple Calendar and Google both show it) count
/// once. The match is confident when exactly one candidate has at least half
/// of its own scheduled length inside the recording, and the chosen copy is
/// that candidate. A meeting that ran long still passes; a recording that
/// covers less than half of its event never does.
///
/// # Errors
/// [`NotEligible::EventNotFound`] when no copy with that source and id
/// overlaps the recording; [`NotEligible::NoConfidentMatch`] otherwise when
/// the match is not confident.
pub fn confident_match<'a>(
    events: &'a [ScheduledEvent],
    start_ms: i64,
    end_ms: i64,
    source: &str,
    uid: &str,
) -> Result<&'a ScheduledEvent, NotEligible> {
    let end_ms = end_ms.max(start_ms);
    // The stored copy, as this window shows it. A recurring event in
    // EventKit gives every occurrence the series' identifier, so the one
    // that overlaps the recording most is the instance that was recorded.
    let chosen = events
        .iter()
        .filter(|e| e.source == source && e.uid == uid)
        .filter(|e| touches(e, start_ms, end_ms))
        .max_by_key(|e| e.overlap(start_ms, end_ms))
        .ok_or(NotEligible::EventNotFound)?;
    let recorded = (end_ms - start_ms).max(1);
    let candidate = |e: &ScheduledEvent| e.eligible() && !e.is_long_block(recorded);
    if !candidate(chosen) || !covered(chosen, start_ms, end_ms) {
        return Err(NotEligible::NoConfidentMatch);
    }
    // Copies of one invite count once; any other covered candidate is a
    // rival, and with a rival the recording could have been either.
    let chosen_key = invite_key(chosen);
    let rival = events
        .iter()
        .filter(|e| candidate(e) && covered(e, start_ms, end_ms))
        .any(|e| invite_key(e) != chosen_key);
    if rival {
        return Err(NotEligible::NoConfidentMatch);
    }
    Ok(chosen)
}

/// Which invite an event is a copy of: its iCalendar UID and start, so two
/// occurrences of one series stay apart. Without a UID, only itself.
fn invite_key(e: &ScheduledEvent) -> (bool, &str, &str, i64) {
    match e.ical_uid.as_deref().filter(|u| !u.is_empty()) {
        Some(ical) => (true, "", ical, e.start_ms),
        None => (false, e.source, e.uid.as_str(), e.start_ms),
    }
}

/// Whether the event touches the recording at all.
const fn touches(e: &ScheduledEvent, start_ms: i64, end_ms: i64) -> bool {
    e.start_ms <= end_ms && start_ms <= e.end_ms
}

/// Whether at least half of the event's own scheduled length is inside the
/// recording. A zero-length event counts when it falls inside it.
fn covered(e: &ScheduledEvent, start_ms: i64, end_ms: i64) -> bool {
    let length = e.end_ms - e.start_ms;
    if length <= 0 {
        return length == 0 && start_ms <= e.start_ms && e.start_ms <= end_ms;
    }
    e.overlap(start_ms, end_ms).saturating_mul(2) >= length
}

/// Read `calendar` for the recording `[start_ms, end_ms]` and apply
/// [`confident_match`], giving up after `timeout`.
///
/// Every store must answer in full ([`CalendarLookup::events_overlapping_complete`]):
/// an event that could not be seen could have been a rival.
///
/// # Errors
/// [`NotEligible::CalendarUnavailable`] when the calendar timed out or could
/// not be read in full; otherwise as [`confident_match`].
pub fn lookup_confident(
    calendar: Arc<dyn CalendarLookup>,
    source: &str,
    uid: &str,
    start_ms: i64,
    end_ms: i64,
    timeout: Duration,
) -> Result<ScheduledEvent, NotEligible> {
    let end_ms = end_ms.max(start_ms);
    let store = calendar.source();
    let (tx, rx) = mpsc::sync_channel(1);
    // Detached, as in `lookup_match`: a store that never answers must not
    // hold the caller, and the thread ends whenever it does answer.
    let spawned = std::thread::Builder::new()
        .name("fotw-release-calendar".into())
        .spawn(move || {
            // The same window the matcher read when it named the meeting.
            let answer = calendar.events_overlapping_complete(
                start_ms.saturating_sub(LATE_GRACE_MS),
                end_ms.saturating_add(EARLY_GRACE_MS),
            );
            let _ = tx.send(answer);
        });
    if spawned.is_err() {
        return Err(NotEligible::CalendarUnavailable);
    }
    let Ok(Ok(mut events)) = rx.recv_timeout(timeout) else {
        return Err(NotEligible::CalendarUnavailable);
    };
    for e in &mut events {
        if e.source.is_empty() {
            e.source = store;
        }
    }
    confident_match(&events, start_ms, end_ms, source, uid).cloned()
}

/// One person a release may go to (D4's frozen recipient).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    /// Lowercase, in an allowed domain.
    pub address: String,
    /// The calendar's display name, when it has one.
    pub display_name: Option<String>,
    /// Their answer to the invitation. Never `declined`.
    pub response: Response,
    /// The person recording.
    pub is_self: bool,
}

/// The recipients of one event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recipients {
    /// Everyone who may receive it, in the calendar's order.
    pub list: Vec<Recipient>,
    /// More than [`MAX_RECIPIENTS`]: reason `many_recipients`, which forces
    /// review. The list is not cut short.
    pub many_recipients: bool,
}

/// Who a release of `event` goes to, given `allowed_domains`.
///
/// Only this copy's attendees, its organiser included, and the user when
/// they are on it. Rooms, resources, groups and anyone who declined are left
/// out. Addresses are lowercased and kept only when their domain is exactly
/// an allowed one: `sub.example.com` is not `example.com`.
#[must_use]
pub fn recipients(event: &ScheduledEvent, allowed_domains: &[String]) -> Recipients {
    // One entry per address, in the calendar's order. An address listed
    // twice is left out if any listing of it is excluded: someone who
    // declined stays out even if their organiser entry says nothing.
    let mut seen: Vec<(String, bool, &Attendee)> = Vec::new();
    for attendee in &event.attendees {
        let Some(address) = attendee.address.as_deref().and_then(normalize_address) else {
            continue;
        };
        let out = excluded(attendee.kind, attendee.response);
        if let Some(entry) = seen.iter_mut().find(|(a, _, _)| *a == address) {
            entry.1 |= out;
            if attendee.is_self && !entry.2.is_self {
                entry.2 = attendee;
            }
        } else {
            seen.push((address, out, attendee));
        }
    }
    let list: Vec<Recipient> = seen
        .into_iter()
        .filter(|(address, out, _)| !out && address_allowed(address, allowed_domains))
        .map(|(address, _, a)| Recipient {
            address,
            display_name: a
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
            response: a.response,
            is_self: a.is_self,
        })
        .collect();
    Recipients {
        many_recipients: list.len() > MAX_RECIPIENTS,
        list,
    }
}

/// `raw` as a lowercase address, or `None` when it is not one plain address.
///
/// Exactly one `@`, both sides non-empty, and nothing that could split or
/// wrap an address on its way to a mail command: whitespace, control
/// characters, commas, semicolons, quotes, brackets or backslashes.
#[must_use]
pub fn normalize_address(raw: &str) -> Option<String> {
    let address = raw.trim().to_lowercase();
    let (local, domain) = address.split_once('@')?;
    let plain = |part: &str| {
        !part.is_empty()
            && part.chars().all(|c| {
                !c.is_whitespace()
                    && !c.is_control()
                    && !matches!(
                        c,
                        '@' | ',' | ';' | '<' | '>' | '"' | '(' | ')' | '[' | ']' | '\\'
                    )
            })
    };
    (plain(local) && plain(domain)).then_some(address)
}

/// Whether `address` (already normalised) is in one of `allowed_domains`,
/// exactly. Subdomains do not match. Also usable for the send-time re-filter
/// (D13).
#[must_use]
pub fn address_allowed(address: &str, allowed_domains: &[String]) -> bool {
    let Some((_, domain)) = address.rsplit_once('@') else {
        return false;
    };
    allowed_domains
        .iter()
        .any(|allowed| !allowed.is_empty() && allowed.eq_ignore_ascii_case(domain))
}

/// Rooms, resources, groups and anyone who declined never receive a release.
fn excluded(kind: AttendeeKind, response: Response) -> bool {
    kind != AttendeeKind::Person || response == Response::Declined
}

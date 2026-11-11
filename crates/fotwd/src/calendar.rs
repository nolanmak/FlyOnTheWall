//! Naming a recording after the calendar event it overlaps (#295).
//!
//! When a recording overlaps an event on the user's calendar, the meeting is
//! saved under that event's title. Otherwise nothing changes: the dated
//! placeholder goes in, and enrichment names it from the transcript as before.
//!
//! # Why a calendar title is never replaced by enrichment
//!
//! Nothing here touches `enrich`. A title that is not empty, not the
//! `Untitled recording` placeholder and not the receipt's `minted_title` is
//! already "a name a person chose" to enrichment's guard, so a calendar title
//! is kept exactly as a rename would be, and a later rename of it is kept too.
//! The receipt is deliberately left without the calendar title: writing it
//! there would make the title replaceable.
//!
//! # What is read, and what is kept
//!
//! A lookup reads the title, times, all-day flag, status and the user's own
//! response for events near the recording. Only the chosen event's title and
//! identifier are stored. Attendees, notes, location and links are never read.
//! The title then behaves like any other meeting title.
//!
//! # A calendar never holds up a save
//!
//! The lookup runs on its own thread with a deadline ([`LOOKUP_TIMEOUT`]). A
//! calendar that hangs, errors or is not authorised costs the meeting its
//! calendar name and nothing else.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

#[cfg(target_os = "macos")]
pub mod eventkit;

/// How long before an event's start a recording may begin and still be
/// counted as that event.
pub const EARLY_GRACE_MS: i64 = 10 * 60_000;

/// How long after an event's end a recording may run, or begin, and still be
/// counted as that event: late starts and overruns.
pub const LATE_GRACE_MS: i64 = 30 * 60_000;

/// The longest a save waits on the calendar.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// The user's own response to an invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Participation {
    /// Accepted.
    Accepted,
    /// Tentative, or "maybe".
    Tentative,
    /// Declined. Never chosen.
    Declined,
    /// No invitation, or no answer: the user's own events.
    Unknown,
}

/// An event on the user's calendar, in wall-clock epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEvent {
    /// A stable identifier from the calendar store.
    pub uid: String,
    /// The event title, as the calendar has it.
    pub title: String,
    /// Start, epoch milliseconds UTC.
    pub start_ms: i64,
    /// End, epoch milliseconds UTC.
    pub end_ms: i64,
    /// All-day events are never chosen: they are days, not meetings.
    pub all_day: bool,
    /// Canceled events are never chosen.
    pub canceled: bool,
    /// The user's own response.
    pub participation: Participation,
}

impl ScheduledEvent {
    fn eligible(&self) -> bool {
        !self.all_day
            && !self.canceled
            && self.participation != Participation::Declined
            && self.end_ms >= self.start_ms
            && !self.title.trim().is_empty()
    }

    /// Whether the event, widened by the grace windows, touches the recording.
    fn near(&self, start_ms: i64, end_ms: i64) -> bool {
        self.start_ms.saturating_sub(EARLY_GRACE_MS) <= end_ms
            && start_ms <= self.end_ms.saturating_add(LATE_GRACE_MS)
    }

    /// Real overlap with the recording, without the grace windows.
    fn overlap(&self, start_ms: i64, end_ms: i64) -> i64 {
        (self.end_ms.min(end_ms) - self.start_ms.max(start_ms)).max(0)
    }
}

/// What a matched meeting is saved with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarLink {
    /// The cleaned event title: the meeting's title.
    pub title: String,
    /// The event's identifier, stored as `meetings.calendar_uid`.
    pub uid: String,
    /// Which calendar store answered, stored as `meetings.calendar_source`.
    pub source: String,
}

/// Where events come from.
pub trait CalendarLookup: Send + Sync {
    /// A short name for the store, saved beside the event identifier.
    fn source(&self) -> &'static str;

    /// Events that overlap `[start_ms, end_ms]` at all.
    ///
    /// # Errors
    /// The store could not be read. The message must not contain event
    /// content: it is logged.
    fn events_overlapping(&self, start_ms: i64, end_ms: i64)
    -> Result<Vec<ScheduledEvent>, String>;
}

/// No calendar: access denied, not yet granted, or not macOS.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unavailable;

impl CalendarLookup for Unavailable {
    fn source(&self) -> &'static str {
        "none"
    }

    fn events_overlapping(&self, _: i64, _: i64) -> Result<Vec<ScheduledEvent>, String> {
        Ok(Vec::new())
    }
}

/// A calendar holding a fixed list of events. For tests.
#[derive(Debug, Clone)]
pub struct Fixed {
    source: &'static str,
    events: Vec<ScheduledEvent>,
}

impl Fixed {
    /// A calendar holding exactly `events`.
    #[must_use]
    pub const fn new(source: &'static str, events: Vec<ScheduledEvent>) -> Self {
        Self { source, events }
    }
}

impl CalendarLookup for Fixed {
    fn source(&self) -> &'static str {
        self.source
    }

    fn events_overlapping(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<ScheduledEvent>, String> {
        Ok(self
            .events
            .iter()
            .filter(|e| e.start_ms <= end_ms && start_ms <= e.end_ms)
            .cloned()
            .collect())
    }
}

/// Why a lookup produced no answer at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupError {
    /// The store did not answer within the deadline.
    #[error("calendar lookup timed out")]
    TimedOut,
    /// The store answered with an error.
    #[error("calendar lookup failed: {0}")]
    Failed(String),
}

/// The event that names a recording spanning `[start_ms, end_ms]`, if any.
///
/// Candidates are eligible events whose `[start − 10 min, end + 30 min]` window
/// touches the recording. The one with the most real overlap wins; ties go to
/// the nearest start, then an accepted invitation, then the earliest start,
/// then the identifier, so the same calendar always gives the same answer.
#[must_use]
pub fn match_event(
    events: &[ScheduledEvent],
    start_ms: i64,
    end_ms: i64,
) -> Option<&ScheduledEvent> {
    let end_ms = end_ms.max(start_ms);
    events
        .iter()
        .filter(|e| e.eligible() && e.near(start_ms, end_ms))
        .min_by(|a, b| {
            b.overlap(start_ms, end_ms)
                .cmp(&a.overlap(start_ms, end_ms))
                .then_with(|| {
                    (a.start_ms - start_ms)
                        .unsigned_abs()
                        .cmp(&(b.start_ms - start_ms).unsigned_abs())
                })
                .then_with(|| {
                    let accepted = |e: &ScheduledEvent| e.participation != Participation::Accepted;
                    accepted(a).cmp(&accepted(b))
                })
                .then_with(|| a.start_ms.cmp(&b.start_ms))
                .then_with(|| a.uid.cmp(&b.uid))
        })
}

/// An event title made safe to store as a meeting title.
///
/// The first non-blank line, control characters turned to spaces, whitespace
/// collapsed, and cut to the rename limit at a character boundary. `None` when
/// nothing is left.
#[must_use]
pub fn clean_event_title(raw: &str) -> Option<String> {
    let line = raw.lines().find(|l| !l.trim().is_empty())?;
    let spaced: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = collapsed
        .chars()
        .take(fotw_web::documents::MAX_TITLE_CHARS)
        .collect();
    let cut = cut.trim_end();
    (!cut.is_empty()).then(|| cut.to_owned())
}

/// Ask `calendar` which event a recording spanning `[start_ms, end_ms]` was,
/// giving up after `timeout`.
///
/// # Errors
/// The store timed out or failed. Neither error carries event content.
pub fn lookup_match(
    calendar: Arc<dyn CalendarLookup>,
    start_ms: i64,
    end_ms: i64,
    timeout: Duration,
) -> Result<Option<CalendarLink>, LookupError> {
    let end_ms = end_ms.max(start_ms);
    let source = calendar.source();
    let (tx, rx) = mpsc::sync_channel(1);
    // Detached on purpose: a store that never answers must not hold the save,
    // and the thread exits by itself whenever it does answer.
    let spawned = std::thread::Builder::new()
        .name("fotw-calendar".into())
        .spawn(move || {
            let answer = calendar.events_overlapping(
                start_ms.saturating_sub(LATE_GRACE_MS),
                end_ms.saturating_add(EARLY_GRACE_MS),
            );
            let _ = tx.send(answer);
        });
    if let Err(e) = spawned {
        return Err(LookupError::Failed(format!("could not start lookup: {e}")));
    }
    let events = match rx.recv_timeout(timeout) {
        Ok(answer) => answer.map_err(LookupError::Failed)?,
        Err(mpsc::RecvTimeoutError::Timeout) => return Err(LookupError::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(LookupError::Failed("lookup ended without an answer".into()));
        }
    };
    // A title that cleans to nothing is no name at all, so it is not offered
    // to the matcher; an emptier rival must not win because of it.
    let usable: Vec<ScheduledEvent> = events
        .into_iter()
        .filter_map(|mut e| {
            e.title = clean_event_title(&e.title)?;
            Some(e)
        })
        .collect();
    Ok(
        match_event(&usable, start_ms, end_ms).map(|e| CalendarLink {
            title: e.title.clone(),
            uid: e.uid.clone(),
            source: source.to_owned(),
        }),
    )
}

/// The calendar this machine can read right now.
///
/// EventKit on macOS. Whether access is granted is checked on every lookup,
/// so granting or revoking it in System Settings takes effect without a
/// restart.
#[must_use]
pub fn system() -> Arc<dyn CalendarLookup> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(eventkit::EventKitCalendar)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(Unavailable)
    }
}

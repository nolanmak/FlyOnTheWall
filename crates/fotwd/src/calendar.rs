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
//! A lookup reads the title, times, all-day flag, status and attendee list
//! for events near the recording, from Apple Calendar and, when the user
//! connected one, Google accounts through Composio (see [`composio`] for what
//! that sends off the machine). The chosen event's title, identifier and
//! invitee *names* are kept: names fix misheard names
//! (`crate::invitees`). For naming, addresses are only used to derive a name
//! and are never stored. Notes, location and links are never read.
//!
//! Attendee release (#326) is the one exception: with it turned on, the
//! addresses of the chosen event's attendees in an allowed domain, with their
//! response, are stored with the meeting's release so it can be emailed to
//! them ([`release`]). Addresses outside the allowed domains are never stored.
//! With release off, nothing changes.
//!
//! # A calendar never holds up a save
//!
//! The lookup runs on its own thread with a deadline ([`LOOKUP_TIMEOUT`]). A
//! calendar that hangs, errors or is not authorised costs the meeting its
//! calendar name and nothing else.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

pub mod composio;
#[cfg(target_os = "macos")]
pub mod eventkit;
pub mod release;

/// How long before an event's start a recording may begin and still be
/// counted as that event.
pub const EARLY_GRACE_MS: i64 = 10 * 60_000;

/// How long after an event's end a recording may run, or begin, and still be
/// counted as that event: late starts and overruns.
pub const LATE_GRACE_MS: i64 = 30 * 60_000;

/// Events longer than this, and more than four times the recording, are
/// blocks rather than meetings and never name one.
pub const LONG_BLOCK_MS: i64 = 2 * 60 * 60_000;

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

/// An attendee's answer to an invitation, as the calendar has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// Accepted.
    Accepted,
    /// Tentative, or "maybe".
    Tentative,
    /// Invited and not answered yet.
    NeedsAction,
    /// Declined.
    Declined,
    /// Anything else, or nothing said.
    Unknown,
}

/// What an attendee is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttendeeKind {
    /// A person, or anything the calendar does not say otherwise about.
    Person,
    /// A group or mailing list, when the calendar marks it as one.
    Group,
    /// A room or other bookable resource.
    Resource,
}

/// One attendee of an event, as read from the calendar.
///
/// Held in memory only. Attendee release ([`release`]) is the one reader of
/// the address, and it keeps only addresses in an allowed domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    /// The address as the calendar has it (EventKit: from a `mailto:` URL
    /// only). `None` when the calendar gives none.
    pub address: Option<String>,
    /// The display name, when the calendar has one.
    pub display_name: Option<String>,
    /// Their answer to the invitation.
    pub response: Response,
    /// Person, group or resource.
    pub kind: AttendeeKind,
    /// This is the calendar's own user: the person recording.
    pub is_self: bool,
    /// This is the event's organiser.
    pub is_organizer: bool,
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
    /// Which calendar store it came from, saved as `meetings.calendar_source`.
    /// Empty means "whichever lookup returned it".
    pub source: &'static str,
    /// Other people invited, by name (never by address): the calendar's
    /// display name, or one derived from the address. Excludes the user.
    pub invitees: Vec<String>,
    /// The event's iCalendar UID, shared by every copy of the same invite in
    /// different calendars. Not the same thing as [`Self::uid`].
    pub ical_uid: Option<String>,
    /// Everyone on the invite, with addresses, including the user and the
    /// organiser. Only attendee release reads this; naming uses `invitees`.
    pub attendees: Vec<Attendee>,
}

impl ScheduledEvent {
    /// Whether the event may name a recording at all: not all-day, not
    /// canceled, not declined, not reversed, and titled.
    pub(crate) fn eligible(&self) -> bool {
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

    /// A block many times longer than a recording of `recorded_ms` (a day of
    /// "Code & Coffee", a focus block): never the meeting that was recorded.
    pub(crate) fn is_long_block(&self, recorded_ms: i64) -> bool {
        let length = self.end_ms - self.start_ms;
        length > LONG_BLOCK_MS && length > recorded_ms.saturating_mul(4)
    }

    /// Overlap over the combined span of event and recording, in millionths.
    fn fit(&self, start_ms: i64, end_ms: i64) -> i64 {
        let union = (self.end_ms.max(end_ms) - self.start_ms.min(start_ms)).max(1);
        self.overlap(start_ms, end_ms).saturating_mul(1_000_000) / union
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
    /// The event's invitees by name, kept with the meeting to fix misheard
    /// names (`crate::invitees`).
    pub invitees: Vec<String>,
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
    /// Like [`Self::events_overlapping`], but an error unless every part of
    /// the store answered. Naming is happy with what it can get; attendee
    /// release must not decide an event is the only candidate because a
    /// rival's calendar was down.
    ///
    /// # Errors
    /// Any part of the store could not be read. No event content.
    fn events_overlapping_complete(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<ScheduledEvent>, String> {
        self.events_overlapping(start_ms, end_ms)
    }
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

/// Several calendars read as one: Apple Calendar and any Google accounts
/// connected through Composio.
///
/// A store that fails is skipped so the others still name the recording; only
/// when every store fails is the lookup an error.
pub struct Many {
    stores: Vec<Arc<dyn CalendarLookup>>,
    deadline: Duration,
}

impl Many {
    /// All of `stores`, each given [`MANY_DEADLINE`] to answer.
    #[must_use]
    pub fn new(stores: Vec<Arc<dyn CalendarLookup>>) -> Self {
        Self::with_deadline(stores, MANY_DEADLINE)
    }

    /// All of `stores`, read side by side; a store that has not answered by
    /// `deadline` is left out rather than holding up the others.
    #[must_use]
    pub fn with_deadline(stores: Vec<Arc<dyn CalendarLookup>>, deadline: Duration) -> Self {
        Self { stores, deadline }
    }
}

/// How long [`Many`] waits for its slowest store: under
/// [`NETWORK_LOOKUP_TIMEOUT`], so Apple Calendar's answer survives a Google
/// account that never answers.
pub const MANY_DEADLINE: Duration = Duration::from_secs(6);

impl CalendarLookup for Many {
    fn source(&self) -> &'static str {
        "many"
    }

    fn events_overlapping(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<ScheduledEvent>, String> {
        self.read(start_ms, end_ms, false)
    }

    /// Every store must answer, in full and in time.
    fn events_overlapping_complete(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<ScheduledEvent>, String> {
        self.read(start_ms, end_ms, true)
    }
}

impl Many {
    /// Read every store side by side. `complete`: any store failing or
    /// missing the deadline fails the read, instead of being skipped.
    fn read(
        &self,
        start_ms: i64,
        end_ms: i64,
        complete: bool,
    ) -> Result<Vec<ScheduledEvent>, String> {
        let until = std::time::Instant::now() + self.deadline;
        let (tx, rx) = mpsc::channel();
        for store in &self.stores {
            let (store, tx) = (Arc::clone(store), tx.clone());
            // Detached: a store past the deadline finishes on its own.
            let _ = std::thread::Builder::new()
                .name("fotw-calendar-store".into())
                .spawn(move || {
                    let answer = if complete {
                        store.events_overlapping_complete(start_ms, end_ms)
                    } else {
                        store.events_overlapping(start_ms, end_ms)
                    };
                    let _ = tx.send((store.source(), answer));
                });
        }
        drop(tx);
        let mut events = Vec::new();
        let (mut answered, mut failures) = (0, Vec::new());
        while answered < self.stores.len() {
            let left = until.saturating_duration_since(std::time::Instant::now());
            let Ok((source, answer)) = rx.recv_timeout(left) else {
                break;
            };
            answered += 1;
            match answer {
                Ok(found) => events.extend(found.into_iter().map(|mut e| {
                    if e.source.is_empty() {
                        e.source = source;
                    }
                    e
                })),
                Err(e) => failures.push(format!("{source}: {e}")),
            }
        }
        let succeeded = answered - failures.len();
        if complete && succeeded < self.stores.len() {
            if failures.is_empty() {
                return Err("a calendar did not answer in time".into());
            }
            return Err(failures.join("; "));
        }
        if !self.stores.is_empty() && succeeded == 0 {
            if failures.is_empty() {
                return Err("no calendar answered in time".into());
            }
            return Err(failures.join("; "));
        }
        Ok(events)
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
/// touches the recording, leaving out blocks over two hours and four times the
/// recording. The best fit (overlap over combined span) wins, then the most
/// real overlap; ties go to
/// the nearest start, then a copy that lists invitees, then an accepted
/// invitation, then the earliest start,
/// then the identifier, so the same calendar always gives the same answer.
#[must_use]
pub fn match_event(
    events: &[ScheduledEvent],
    start_ms: i64,
    end_ms: i64,
) -> Option<&ScheduledEvent> {
    let end_ms = end_ms.max(start_ms);
    let recorded = (end_ms - start_ms).max(1);
    events
        .iter()
        .filter(|e| e.eligible() && e.near(start_ms, end_ms))
        // A block many times longer than the recording (a day of "Code &
        // Coffee", a focus block) is not the meeting that was recorded.
        .filter(|e| !e.is_long_block(recorded))
        .min_by(|a, b| {
            // How well the event and the recording line up: overlap over
            // their combined span. A 30-minute invite fits a 43-minute call
            // far better than the 8-hour block it sits inside.
            b.fit(start_ms, end_ms)
                .cmp(&a.fit(start_ms, end_ms))
                .then_with(|| {
                    b.overlap(start_ms, end_ms)
                        .cmp(&a.overlap(start_ms, end_ms))
                })
                .then_with(|| {
                    (a.start_ms - start_ms)
                        .unsigned_abs()
                        .cmp(&(b.start_ms - start_ms).unsigned_abs())
                })
                // The same meeting in two calendars: the copy that knows who
                // was invited (Google's own, not Apple's mirror of it).
                .then_with(|| a.invitees.is_empty().cmp(&b.invitees.is_empty()))
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
    // to the matcher; an emptier rival must not win because of it. Nor is one
    // that starts like the placeholder: enrichment reads that prefix as
    // replaceable, so it would be silently overwritten later.
    let usable: Vec<ScheduledEvent> = events
        .into_iter()
        .filter_map(|mut e| {
            e.title = clean_event_title(&e.title)?;
            (!e.title.starts_with(crate::enrich::FALLBACK_PREFIX)).then_some(e)
        })
        .collect();
    Ok(
        match_event(&usable, start_ms, end_ms).map(|e| CalendarLink {
            title: e.title.clone(),
            uid: e.uid.clone(),
            source: if e.source.is_empty() {
                source
            } else {
                e.source
            }
            .to_owned(),
            invitees: pooled_invitees(&usable, e),
        }),
    )
}

/// The longest a save waits when a Google account is connected through
/// Composio: a network round trip per account, not a local read.
pub const NETWORK_LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);

/// Every calendar a recording in the library at `root` may be named from, and
/// how long a save should wait for them.
///
/// Apple Calendar always; Composio as well when the user connected a Google
/// account there.
#[must_use]
pub fn for_library_root(root: &std::path::Path) -> (Arc<dyn CalendarLookup>, Duration) {
    let google = crate::open_library(root)
        .ok()
        .and_then(|db| composio::for_library(&db));
    match google {
        Some(google) => (
            Arc::new(Many::new(vec![system(), Arc::new(google)])),
            NETWORK_LOOKUP_TIMEOUT,
        ),
        None => (system(), LOOKUP_TIMEOUT),
    }
}

/// The chosen event's invitees, plus those of the same event as another
/// calendar store has it (same title and start, or the same span). A Google invite shown in Apple
/// Calendar often has no usable names; Google's own copy does.
fn pooled_invitees(events: &[ScheduledEvent], chosen: &ScheduledEvent) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    // Same title and start, or exactly the same span: the same meeting as
    // another calendar store has it, possibly under its own title.
    let same = events.iter().filter(|e| {
        e.start_ms == chosen.start_ms && (e.title == chosen.title || e.end_ms == chosen.end_ms)
    });
    for name in std::iter::once(chosen)
        .chain(same)
        .flat_map(|e| &e.invitees)
    {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        // The chosen event knows nobody (an Apple block for a meeting whose
        // invite lives in Google, often with a different end). Borrow from
        // an event that starts within a few minutes of it and does list
        // invitees: the same meeting as another calendar has it.
        if let Some(twin) = events
            .iter()
            .filter(|e| !e.invitees.is_empty())
            .filter(|e| (e.start_ms - chosen.start_ms).abs() <= SAME_MEETING_START_MS)
            .min_by_key(|e| (e.start_ms - chosen.start_ms).abs())
        {
            names.clone_from(&twin.invitees);
        }
    }
    names
}

/// How close two events' starts must be to count as the same meeting when
/// borrowing invitees.
const SAME_MEETING_START_MS: i64 = 5 * 60_000;

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

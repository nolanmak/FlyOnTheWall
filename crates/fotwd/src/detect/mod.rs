//! Meeting detection — which **arms** the shell and never records (MTG-03,
//! MTG-04, CON-01).
//!
//! ```text
//!   Core Audio process list ──► Detector::poll ──► Detection::Arm
//!                                                        │
//!                                                        ▼
//!                                       ShellInput::MeetingDetected
//!                                                        │
//!                                          (a person clicks Start)
//!                                                        ▼
//!                                             ShellEffect::StartCapture
//! ```
//!
//! Nothing in this module can reach the right-hand end of that diagram. The
//! only thing it produces is a [`Detection`], and the only thing the shell
//! does with one is put a prompt on screen. That is CON-01, and it is a legal
//! position: starting from a signal instead of from a person is the conduct
//! pleaded in *Chamberlain v. Granola* at $5,000 per recorded participant.
//!
//! # The conjunction, and why it is not tunable
//!
//! Arming requires **(a known conferencing app is running) AND (that same app
//! holds the microphone)**, or **(a known conferencing app is running) AND (a
//! calendar event is in progress)**. Never one signal alone.
//!
//! This is a consent requirement rather than a UX preference. Zoom and Teams
//! idle in the background on most installs; a detector that armed on process
//! presence would prompt several times a day, and the all-party jurisdiction
//! warning (CON-05) rides on the same surface as the prompt. A user habituated
//! into dismissing the prompt has been habituated into dismissing the warning.
//!
//! # False positives and negatives, honestly
//!
//! **There is no measured false-positive rate.** Nobody has run this against a
//! week of real meetings, and a number invented here would be worse than
//! none. What is known:
//!
//! *False positives this design still has*
//! - A dedicated conferencing app holding the mic for something that is not a
//!   meeting: Zoom's audio test panel, a Slack voice clip, a Discord push-to-talk
//!   channel joined without anyone speaking. All are genuine microphone use in
//!   a conferencing app, so no signal available here separates them.
//! - A browser in a call *of any kind* — a Meet call and a browser-based voice
//!   recorder look identical. Browsers therefore require both input and output
//!   ([`catalog::ConferencingApp::browser`]), which removes recorders but not
//!   web-based phones.
//! - With a calendar event in progress, an idle-but-running conferencing app
//!   is enough. That is the documented fallback for the Bluetooth case, and it
//!   fires on someone who has Zoom open during a meeting they are attending in
//!   person.
//!
//! *False negatives, which are the safer failure*
//! - A conferencing app that does not appear in [`catalog::APPS`], or whose
//!   bundle id changed. It is simply never detected.
//! - Anything where the app never reports `IsRunningInput`. Whether a muted
//!   participant still holds the input is **not verified** for any app here —
//!   most implement mute in software and keep the device open, but that is an
//!   assumption, not a measurement.
//! - Bluetooth microphones (issue #22). The mitigation is the calendar
//!   fallback, and the calendar is not wired to EventKit yet (MTG-01), so on a
//!   real Mac today that fallback is dormant and AirPods users may see no
//!   prompts at all. **They can still press record.** Every failure in this
//!   module has that as its floor.
//!
//! # Nothing here leaves the machine
//!
//! [`DetectionStats`] is a plain in-process counter, kept so the conjunction
//! can be tuned against a real week of use without telemetry. It is never
//! serialised to a network transport; the only export is a line printed by
//! `fotwd detect`.

pub mod catalog;

use std::collections::BTreeSet;
use std::time::Duration;

use fotw_audio::activity::{ActivitySnapshot, AudioClient};
use fotw_shell::{DetectedMeeting, Monotonic};

use crate::consent::{JurisdictionSignals, Rules};

pub use catalog::ConferencingApp;

/// A calendar event that is happening right now.
///
/// Deliberately minimal, and deliberately not an EventKit type: MTG-01 is a
/// separate piece of work, and detection must be testable and reviewable
/// without it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CalendarEvent {
    /// The event title, shown in the prompt.
    pub title: String,
    /// Attendee email domains, which feed the consent engine's jurisdiction
    /// signals (CON-05).
    pub attendee_domains: Vec<String>,
    /// The event's timezone, likewise.
    pub timezone: Option<String>,
}

impl CalendarEvent {
    /// An event with a title and nothing else.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// Attach attendee domains for the jurisdiction check.
    #[must_use]
    pub fn with_attendee_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.attendee_domains
            .extend(domains.into_iter().map(Into::into));
        self
    }

    /// Attach the event timezone.
    #[must_use]
    pub fn with_timezone(mut self, tz: impl Into<String>) -> Self {
        self.timezone = Some(tz.into());
        self
    }
}

/// Where "is a meeting scheduled right now" comes from.
pub trait CalendarSource {
    /// The event covering `now`, if any.
    fn current_event(&self, now: Monotonic) -> Option<CalendarEvent>;
}

/// No calendar access — the shipped default until EventKit lands (MTG-01).
///
/// The app is fully usable this way; it only loses the fallback that covers
/// Bluetooth microphones.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCalendar;

impl CalendarSource for NoCalendar {
    fn current_event(&self, _now: Monotonic) -> Option<CalendarEvent> {
        None
    }
}

/// A calendar that always reports the same event. For tests and for
/// `fotwd detect --pretend-calendar`.
#[derive(Debug, Clone)]
pub struct FixedCalendar {
    event: CalendarEvent,
}

impl FixedCalendar {
    /// A calendar permanently inside `event`.
    #[must_use]
    pub const fn new(event: CalendarEvent) -> Self {
        Self { event }
    }
}

impl CalendarSource for FixedCalendar {
    fn current_event(&self, _now: Monotonic) -> Option<CalendarEvent> {
        Some(self.event.clone())
    }
}

/// What one poll concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// Nothing to say.
    Idle,
    /// A meeting has been detected and the user should be asked. **This is a
    /// request to show a prompt, not to record.**
    Arm(DetectedMeeting),
    /// Already armed, signals unchanged.
    Hold,
    /// Withdraw the prompt: the signals are gone, or the machine state could
    /// not be read.
    Clear,
}

/// Local-only counters, so the conjunction can be tuned without telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetectionStats {
    /// How many times a prompt was raised.
    pub armed: u64,
    /// How many of those the user turned into a recording.
    pub started: u64,
    /// How many were answered "not now".
    pub snoozed: u64,
    /// How many were answered "never for this app".
    pub suppressed: u64,
    /// How many were withdrawn before the user answered at all.
    pub withdrawn: u64,
}

impl DetectionStats {
    /// Prompt-to-start conversion, in `0.0..=1.0`.
    ///
    /// The number that says whether the conjunction is tuned: a rate near zero
    /// means the detector is crying wolf, and a consent surface nobody reads
    /// is worse than no surface at all.
    #[must_use]
    pub fn conversion(&self) -> f64 {
        if self.armed == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "counters this large would mean 10^15 prompts"
        )]
        {
            self.started as f64 / self.armed as f64
        }
    }
}

/// How eagerly to detect.
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// How long the conjunction must hold before a prompt is raised.
    ///
    /// A meeting app takes a moment to settle when a call connects, and a
    /// one-poll blip (a notification sound, a mic check) is not a meeting.
    pub dwell: Duration,
    /// How long "not now" lasts.
    pub snooze: Duration,
    /// How long the signals must be *gone* before the prompt is withdrawn.
    ///
    /// Device switches and app restarts make the flags flicker; withdrawing
    /// instantly would take the prompt away while the user reaches for it.
    pub clear_grace: Duration,
    /// Our own pid, excluded from the evidence: we hold the microphone while
    /// recording, and counting that would make the detector fire off its own
    /// capture.
    pub self_pid: u32,
    /// The user's declared home jurisdiction, for the consent notice attached
    /// to every prompt (CON-05).
    pub home_jurisdiction: String,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            dwell: Duration::from_secs(20),
            snooze: Duration::from_secs(600),
            clear_grace: Duration::from_secs(30),
            self_pid: std::process::id(),
            // The strictest common case rather than a guess, matching
            // `fotwd record`. Being wrong here over-warns, which is the
            // direction CON-05 asks us to be wrong in.
            home_jurisdiction: "US-CA".to_owned(),
        }
    }
}

/// The evidence that made a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    /// The app itself holds the microphone.
    MicHot,
    /// A calendar event is in progress and the app is running.
    Calendar(CalendarEvent),
}

#[derive(Debug, Clone)]
struct Candidate {
    app: &'static ConferencingApp,
    evidence: Evidence,
}

/// The detector.
///
/// Clock-injected like [`fotw_shell::ShellCore`]: every method that needs the
/// time is given it, so a two-hour meeting runs in microseconds and the dwell
/// and snooze windows are tested rather than waited out.
#[derive(Debug)]
pub struct Detector {
    config: DetectorConfig,
    rules: Rules,
    suppressed: BTreeSet<String>,
    /// The app that currently satisfies the conjunction, and since when.
    candidate: Option<(&'static ConferencingApp, Monotonic)>,
    /// The app whose prompt has already been raised — or started, or
    /// otherwise dealt with. Kept until the signals drop so that stopping a
    /// recording mid-call does not immediately re-prompt for the same call.
    handled: Option<String>,
    /// When the handled app's signals went away, for the grace window.
    lost_since: Option<Monotonic>,
    snoozed_until: Option<Monotonic>,
    stats: DetectionStats,
}

impl Detector {
    /// A detector with the shipped rules table.
    #[must_use]
    pub fn new(config: DetectorConfig) -> Self {
        Self {
            config,
            rules: Rules::builtin(),
            suppressed: BTreeSet::new(),
            candidate: None,
            handled: None,
            lost_since: None,
            snoozed_until: None,
            stats: DetectionStats::default(),
        }
    }

    /// The local-only counters.
    #[must_use]
    pub const fn stats(&self) -> &DetectionStats {
        &self.stats
    }

    /// Never prompt for this app again (issue #22's third button).
    pub fn suppress_app(&mut self, app_key: &str) {
        self.suppressed.insert(app_key.to_owned());
        if self.handled.as_deref() == Some(app_key) {
            self.handled = None;
        }
        self.candidate = None;
        self.stats.suppressed += 1;
    }

    /// Whether an app is suppressed.
    #[must_use]
    pub fn is_suppressed(&self, app_key: &str) -> bool {
        self.suppressed.contains(app_key)
    }

    /// Restore suppressions persisted from a previous run.
    pub fn restore_suppressions<I, S>(&mut self, keys: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.suppressed.extend(keys.into_iter().map(Into::into));
    }

    /// Every suppressed app, for persistence.
    pub fn suppressions(&self) -> impl Iterator<Item = &str> {
        self.suppressed.iter().map(String::as_str)
    }

    /// The user said "not now".
    pub fn snooze(&mut self, now: Monotonic) {
        self.snoozed_until = Some(now.plus(self.config.snooze));
        self.handled = None;
        self.candidate = None;
        self.stats.snoozed += 1;
    }

    /// The user started a recording from a prompt.
    ///
    /// Leaves the app marked as handled, so that stopping the recording while
    /// the call is still running does not raise the prompt again a dwell
    /// later.
    pub fn record_started(&mut self) {
        self.stats.started += 1;
    }

    /// Sample the machine.
    ///
    /// `snapshot` is a `Result` because a probe failure and an empty machine
    /// mean opposite things: empty is "no meeting", a failure is "unknown".
    /// Unknown withdraws the prompt — an offer to record a call that may have
    /// ended is worse than one prompt too few.
    pub fn poll(
        &mut self,
        now: Monotonic,
        snapshot: Result<&ActivitySnapshot, &str>,
        calendar: &dyn CalendarSource,
    ) -> Detection {
        let Ok(snapshot) = snapshot else {
            self.candidate = None;
            self.lost_since = None;
            return self.withdraw();
        };

        let found = self.evaluate(snapshot, calendar, now);

        // Track how long the current candidate has held.
        match (&found, &self.candidate) {
            (Some(c), Some((prev, _))) if prev.key == c.app.key => {}
            (Some(c), _) => self.candidate = Some((c.app, now)),
            (None, _) => self.candidate = None,
        }

        // Withdrawal: the app we already prompted for has gone quiet.
        let still_present = found
            .as_ref()
            .is_some_and(|c| Some(c.app.key) == self.handled.as_deref());
        if self.handled.is_some() && !still_present {
            let since = *self.lost_since.get_or_insert(now);
            if now.since(since) >= self.config.clear_grace {
                self.lost_since = None;
                return self.withdraw();
            }
            return Detection::Hold;
        }
        self.lost_since = None;

        if self.handled.is_some() {
            return Detection::Hold;
        }

        // Snoozed: the user said "not now" and meant it.
        if let Some(until) = self.snoozed_until {
            if now < until {
                return Detection::Idle;
            }
            self.snoozed_until = None;
        }

        let Some(candidate) = found else {
            return Detection::Idle;
        };
        let Some((_, since)) = self.candidate else {
            return Detection::Idle;
        };
        if now.since(since) < self.config.dwell {
            return Detection::Idle;
        }

        self.handled = Some(candidate.app.key.to_owned());
        self.stats.armed += 1;
        Detection::Arm(self.describe(&candidate))
    }

    fn withdraw(&mut self) -> Detection {
        if self.handled.take().is_some() {
            self.stats.withdrawn += 1;
            Detection::Clear
        } else {
            Detection::Idle
        }
    }

    /// The conjunction itself.
    fn evaluate(
        &self,
        snapshot: &ActivitySnapshot,
        calendar: &dyn CalendarSource,
        now: Monotonic,
    ) -> Option<Candidate> {
        // Computed once: a calendar lookup may be an EventKit round trip.
        let mut event: Option<Option<CalendarEvent>> = None;

        for app in catalog::APPS {
            if self.suppressed.contains(app.key) {
                continue;
            }
            let clients: Vec<&AudioClient> = snapshot
                .clients
                .iter()
                .filter(|c| c.pid != self.config.self_pid)
                .filter(|c| c.bundle_id.as_deref().is_some_and(|b| app.matches(b)))
                .collect();
            if clients.is_empty() {
                continue;
            }

            let holds_input = clients.iter().any(|c| c.running_input);
            let plays_output = clients.iter().any(|c| c.running_output);
            // A browser holding only the microphone is a voice recorder, a
            // dictation box or a speech demo. A call has both directions.
            let mic_evidence = if app.browser {
                holds_input && plays_output
            } else {
                holds_input
            };

            if mic_evidence {
                return Some(Candidate {
                    app,
                    evidence: Evidence::MicHot,
                });
            }

            let current = event.get_or_insert_with(|| calendar.current_event(now));
            if let Some(e) = current {
                return Some(Candidate {
                    app,
                    evidence: Evidence::Calendar(e.clone()),
                });
            }
        }
        None
    }

    /// Turn a candidate into the thing a person is shown.
    fn describe(&self, candidate: &Candidate) -> DetectedMeeting {
        let app = candidate.app;
        let evidence = match &candidate.evidence {
            Evidence::MicHot => format!("{} is using the microphone", app.name),
            Evidence::Calendar(e) => format!(
                "{} is running and \"{}\" is in your calendar now",
                app.name, e.title
            ),
        };

        let mut signals = JurisdictionSignals::home(&self.config.home_jurisdiction);
        let mut meeting = DetectedMeeting::new(app.key, app.name, evidence);
        if let Evidence::Calendar(e) = &candidate.evidence {
            meeting = meeting.with_title(&e.title);
            signals = signals.with_attendee_domains(&e.attendee_domains);
            if let Some(tz) = &e.timezone {
                signals = signals.with_event_timezone(tz);
            }
        }

        // CON-05: the jurisdiction warning rides on the prompt, and an
        // all-party or contested jurisdiction makes it blocking. Attached
        // here rather than at the click so the user reads it while deciding,
        // not after.
        let escalation = self.rules.escalate(&signals);
        meeting.with_consent_notice(escalation.user_text(), escalation.blocks())
    }
}

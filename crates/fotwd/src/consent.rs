//! Consent as a product feature.
//!
//! Bot-free capture removes **every** platform-generated recording cue. Zoom's
//! mandatory pop-up fires only for host-initiated *Zoom* recordings; nothing
//! appears when a third-party app taps system audio. So the disclosure that
//! would otherwise be the platform's job becomes ours.
//!
//! This is not a compliance appendix. *Chamberlain v. Granola* (N.D. Cal.,
//! filed 2026-07-30) pleads CIPA §§ 631/632 with **§ 637.2(a) statutory
//! damages of $5,000 per violation** — a per-recorded-participant multiplier —
//! and quotes the defendant's own marketing that participants "won't know it's
//! there". The consolidated Otter.ai litigation attacks *outsourcing* consent
//! to customers through a ToS rather than building it into the product. A
//! README paragraph is precisely the failure mode being litigated.
//!
//! # The bias is deliberate
//!
//! The all-party list is genuinely contested — sources disagree on Nevada, and
//! Connecticut and Oregon differ between in-person and electronic
//! communications. **A confidently wrong warning is worse than a general one**,
//! and could itself be relied upon. So contested jurisdictions escalate, and an
//! *unknown* jurisdiction escalates too: failing open would hand the
//! permissive path to exactly the user we know least about.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a jurisdiction treats recording a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentRegime {
    /// One participant's consent suffices.
    OneParty,
    /// Every participant must consent.
    AllParty,
    /// A lawful basis is required but consent is not the only route — the
    /// GDPR-style case, where employee consent is generally invalid anyway
    /// because of the power imbalance.
    Mixed,
    /// Sources disagree, or the rule differs by medium. Treated as all-party.
    Contested,
}

impl ConsentRegime {
    /// Whether this regime demands the blocking modal rather than a reminder.
    #[must_use]
    pub const fn requires_blocking(self) -> bool {
        !matches!(self, Self::OneParty)
    }
}

/// One row of the rules table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Jurisdiction {
    /// `US-CA`, `DE`, …
    pub code: String,
    /// Human-readable name.
    pub name: String,
    /// The regime.
    pub regime: ConsentRegime,
    /// The statute this is based on.
    pub statute: String,
    /// Where to read it.
    pub citation_url: String,
    /// Whether violation is a **criminal** matter rather than only civil.
    /// Germany and France are; most US states are civil for a participant.
    pub criminal: bool,
    /// What is disputed, for contested entries.
    #[serde(default)]
    pub note: String,
    /// ccTLD used to infer this jurisdiction from an attendee's email domain.
    #[serde(default)]
    pub cctld: Option<String>,
}

/// What we know about where the participants are.
#[derive(Debug, Clone, Default)]
pub struct JurisdictionSignals {
    home: Option<String>,
    domains: Vec<String>,
    timezone: Option<String>,
}

impl JurisdictionSignals {
    /// The user's declared home jurisdiction, captured in onboarding.
    ///
    /// Declared rather than geolocated: there is no backend to geolocate
    /// with, and IP would be wrong for remote and travelling users anyway.
    #[must_use]
    pub fn home(code: impl Into<String>) -> Self {
        Self {
            home: Some(code.into()),
            ..Self::default()
        }
    }

    /// Attendee email addresses or domains from the matched calendar event.
    #[must_use]
    pub fn with_attendee_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.domains
            .extend(domains.into_iter().map(|d| d.as_ref().to_ascii_lowercase()));
        self
    }

    /// The calendar event's timezone.
    #[must_use]
    pub fn with_event_timezone(mut self, tz: impl Into<String>) -> Self {
        self.timezone = Some(tz.into());
        self
    }
}

/// What the pre-record sheet should do.
#[derive(Debug, Clone)]
pub enum Escalation {
    /// A single-line reminder. Every resolved jurisdiction is one-party.
    Reminder {
        /// The jurisdictions that were considered.
        jurisdictions: Vec<Jurisdiction>,
    },
    /// A blocking modal naming the jurisdiction and statute, requiring an
    /// explicit "all participants have consented" acknowledgement.
    Blocking {
        /// The jurisdictions that triggered it.
        jurisdictions: Vec<Jurisdiction>,
        /// True when any of them is criminal rather than civil.
        criminal: bool,
    },
}

impl Escalation {
    /// The text to show the user.
    ///
    /// Always ends with the disclaimer. A tool that tells someone their
    /// recording is lawful, and is wrong, has done them real harm — so it
    /// never says that, in either branch.
    #[must_use]
    pub fn user_text(&self) -> String {
        match self {
            Self::Reminder { .. } => "Everyone on this call should know they're being recorded. \
                 This is not legal advice."
                .to_owned(),
            Self::Blocking {
                jurisdictions,
                criminal,
            } => {
                let mut s = String::new();
                if *criminal {
                    s.push_str(
                        "Recording without every participant's consent is a CRIMINAL offence in:\n",
                    );
                } else {
                    s.push_str("These jurisdictions require every participant's consent:\n");
                }
                for j in jurisdictions {
                    s.push_str(&format!(
                        "  • {} — {} ({})\n",
                        j.name, j.statute, j.citation_url
                    ));
                    if !j.note.is_empty() {
                        s.push_str(&format!("    {}\n", j.note));
                    }
                }
                s.push_str("This is not legal advice.");
                s
            }
        }
    }

    /// Whether recording may begin without an explicit acknowledgement.
    #[must_use]
    pub const fn blocks(&self) -> bool {
        matches!(self, Self::Blocking { .. })
    }
}

/// The versioned rules table.
#[derive(Debug, Clone)]
pub struct Rules {
    by_code: BTreeMap<String, Jurisdiction>,
    by_cctld: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RulesFile {
    #[allow(dead_code)]
    version: u32,
    jurisdictions: Vec<Jurisdiction>,
}

impl Rules {
    /// The table shipped with the app.
    ///
    /// Embedded rather than read from disk so a corrupt or missing data file
    /// cannot silently downgrade every user to the one-party path.
    #[must_use]
    pub fn builtin() -> Self {
        const DATA: &str = include_str!("../data/jurisdictions.json");
        let file: RulesFile =
            serde_json::from_str(DATA).expect("the built-in rules table must parse");
        Self::from_list(file.jurisdictions)
    }

    fn from_list(list: Vec<Jurisdiction>) -> Self {
        let mut by_code = BTreeMap::new();
        let mut by_cctld = BTreeMap::new();
        for j in list {
            if let Some(cc) = &j.cctld {
                by_cctld.insert(cc.clone(), j.code.clone());
            }
            by_code.insert(j.code.clone(), j);
        }
        Self { by_code, by_cctld }
    }

    /// Look up one jurisdiction.
    #[must_use]
    pub fn get(&self, code: &str) -> Option<&Jurisdiction> {
        self.by_code.get(code)
    }

    /// Every jurisdiction in the table.
    pub fn all(&self) -> impl Iterator<Item = &Jurisdiction> {
        self.by_code.values()
    }

    /// Just the US states.
    pub fn us_states(&self) -> impl Iterator<Item = &Jurisdiction> {
        self.by_code.values().filter(|j| j.code.starts_with("US-"))
    }

    /// Decide what the pre-record sheet does.
    pub fn escalate(&self, signals: &JurisdictionSignals) -> Escalation {
        let mut resolved: Vec<Jurisdiction> = Vec::new();
        let mut unknown_home = false;

        if let Some(home) = &signals.home {
            match self.get(home) {
                Some(j) => resolved.push(j.clone()),
                // Bias toward over-warning: an unlisted jurisdiction is one we
                // know nothing about, and defaulting it to permissive is the
                // wrong direction to be wrong in.
                None => unknown_home = true,
            }
        }

        for domain in &signals.domains {
            if let Some(cc) = domain.rsplit('.').next()
                && let Some(code) = self.by_cctld.get(cc)
                && let Some(j) = self.get(code)
                && !resolved.iter().any(|r| r.code == j.code)
            {
                resolved.push(j.clone());
            }
        }

        if let Some(tz) = &signals.timezone
            && let Some(j) = self.by_timezone(tz)
            && !resolved.iter().any(|r| r.code == j.code)
        {
            resolved.push(j.clone());
        }

        let blocking: Vec<Jurisdiction> = resolved
            .iter()
            .filter(|j| j.regime.requires_blocking())
            .cloned()
            .collect();

        if unknown_home {
            return Escalation::Blocking {
                criminal: blocking.iter().any(|j| j.criminal),
                jurisdictions: blocking,
            };
        }
        if blocking.is_empty() {
            Escalation::Reminder {
                jurisdictions: resolved,
            }
        } else {
            Escalation::Blocking {
                criminal: blocking.iter().any(|j| j.criminal),
                jurisdictions: blocking,
            }
        }
    }

    /// Map an IANA timezone to a jurisdiction, coarsely.
    ///
    /// Only the region prefix is used. This is a *hint* that can raise the
    /// warning level, never lower it, so being coarse is safe in the direction
    /// that matters.
    fn by_timezone(&self, tz: &str) -> Option<&Jurisdiction> {
        let city = tz.split('/').nth(1)?;
        let code = match city {
            "Berlin" => "DE",
            "Paris" => "FR",
            "London" => "GB",
            "Dublin" => "IE",
            "Amsterdam" => "NL",
            "Madrid" => "ES",
            "Rome" => "IT",
            "Toronto" | "Vancouver" | "Montreal" => "CA",
            "Sydney" | "Melbourne" | "Brisbane" | "Perth" => "AU",
            "Auckland" => "NZ",
            "Tokyo" => "JP",
            "Singapore" => "SG",
            "Kolkata" | "Calcutta" => "IN",
            "Sao_Paulo" => "BR",
            _ => return None,
        };
        self.get(code)
    }
}

/// The disclosure affordances (CON-03).
///
/// Every one of these is a *product feature* rather than documentation,
/// because "we told the customer to get consent" is the theory being
/// litigated against Otter.ai. Nothing here sends anything: the mail path
/// opens the user's own client, which is what keeps the app backend-free.
#[derive(Debug, Clone)]
pub struct DisclosureKit {
    notice: String,
}

impl Default for DisclosureKit {
    fn default() -> Self {
        Self {
            notice: "Heads up — I'm using an AI notetaker to transcribe this call \
                     for my own notes. Let me know if you'd prefer I stop."
                .to_owned(),
        }
    }
}

impl DisclosureKit {
    /// The notice to paste into the meeting chat. Editable by the user.
    #[must_use]
    pub fn chat_notice(&self) -> &str {
        &self.notice
    }

    /// Replace the default notice.
    #[must_use]
    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = notice.into();
        self
    }

    /// A paragraph to paste into a calendar invite description.
    #[must_use]
    pub fn calendar_blurb(&self) -> String {
        "Note: I use an AI notetaker to transcribe meetings for my own notes. \
         No bot joins the call. Recordings and transcripts stay on my own \
         machine. Tell me if you'd rather I didn't for this meeting."
            .to_owned()
    }

    /// What to say out loud at the top of the call.
    #[must_use]
    pub fn verbal_script(&self) -> String {
        "Before we start — I'm recording and transcribing this for my own \
         notes. Any objections?"
            .to_owned()
    }

    /// A `mailto:` asking attendees for consent ahead of the meeting.
    #[must_use]
    pub fn consent_mailto(&self, attendees: &[&str], meeting_title: &str) -> String {
        let to = attendees.join(",");
        let subject = encode(&format!("Recording {meeting_title}"));
        let body = encode(&format!(
            "{}\n\nIf you'd rather I didn't, just say so and I won't.\n",
            self.calendar_blurb()
        ));
        format!("mailto:{to}?subject={subject}&body={body}")
    }
}

/// Percent-encode for a `mailto:` query component.
///
/// Hand-rolled rather than pulling a URL crate for one function. The failure
/// this prevents is silent: an unencoded `&` in a meeting title truncates the
/// body, so the mail opens with the disclosure missing — which is worse than
/// not offering the feature.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

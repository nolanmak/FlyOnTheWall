//! Names from the calendar invite, used to fix misheard names.
//!
//! When a recording matches a calendar event, the invitees' names are kept
//! with the meeting (never their addresses). They are used two ways:
//!
//! * **Corrections, locally.** Capitalised words in the transcript that are
//!   near-misses of an invitee's first name ("Rafael", "Raffiad" for
//!   "Rafiya") become name corrections, the same kind a user adds with
//!   "Remember name correction". The summary, the sharing document and
//!   shared transcript copies then use the invite's spelling; the stored
//!   transcript itself is never rewritten.
//! * **A spelling list for the summary engine**, when [`enabled`]: names a
//!   recogniser mangles too far for string matching ("Raphael") are left to
//!   the model, which is told the invite's spellings as data, not
//!   instructions.
//!
//! The near-miss test is the local vocabulary matcher (Jaro–Winkler with
//! guards for common English words, first-letter and length), applied only
//! to capitalised words, so "rafting" is never a name.

use std::collections::{BTreeMap, HashSet};

use fotw_store::Db;
use fotw_stt::vocabulary::{Correction, Profile};
use fotw_stt::vocabulary_matcher::VocabularyMatcher;
use fotw_web::documents::NameCorrection;
use serde::{Deserialize, Serialize};

/// Where each meeting's invitee names and corrections are kept.
pub const SETTINGS_KEY: &str = "meeting_invitees";

/// Whether invitee names are sent to the summary engine. A missing setting
/// defaults to on. An unreadable setting disables sharing.
pub const ENABLED_KEY: &str = "invitee_names_in_summaries";

/// Most invitees kept for one meeting.
pub const MAX_INVITEES: usize = 40;

/// How close a transcript word must be to an invitee's first name. Lower
/// than the matcher's 0.85 default because the candidates are already
/// limited to capitalised words and to this meeting's invitees.
pub const NAME_THRESHOLD: f64 = 0.80;

/// Longest name kept, matching the name-correction limit.
const MAX_NAME_BYTES: usize = 100;

/// One meeting's invitees and the corrections found for them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeetingInvitees {
    /// Invitee names as the calendar has them, or derived from an address.
    #[serde(default)]
    pub names: Vec<String>,
    /// Transcript spellings to replace with an invitee's name.
    #[serde(default)]
    pub corrections: Vec<NameCorrection>,
}

/// A plain name: letters, spaces, hyphens and apostrophes, as name
/// corrections allow. `None` for anything else.
#[must_use]
pub fn clean_name(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let plain = !collapsed.is_empty()
        && collapsed.len() <= MAX_NAME_BYTES
        && collapsed.chars().any(char::is_alphabetic)
        && collapsed
            .chars()
            .all(|c| c.is_alphabetic() || matches!(c, ' ' | '-' | '\'' | '’'));
    plain.then_some(collapsed)
}

/// A person's name from an address: `kit.marlow@` → "Kit Marlow". `None` for
/// rooms, groups and role addresses.
#[must_use]
pub fn name_from_email(email: &str) -> Option<String> {
    let (local, domain) = email.trim().rsplit_once('@')?;
    let domain = domain.to_ascii_lowercase();
    if domain.ends_with("resource.calendar.google.com")
        || domain.ends_with("group.calendar.google.com")
        || domain.ends_with("group.v.calendar.google.com")
        || domain.ends_with("import.calendar.google.com")
    {
        return None;
    }
    let local = local
        .split('+')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    const ROLES: [&str; 8] = [
        "noreply",
        "no-reply",
        "donotreply",
        "calendar",
        "notifications",
        "admin",
        "info",
        "team",
    ];
    if ROLES
        .iter()
        .any(|role| local == *role || local.starts_with(&format!("{role}-")))
    {
        return None;
    }
    let words: Vec<String> = local
        .split(['.', '_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect();
    clean_name(&words.join(" "))
}

/// An invitee's name: the display name when it is a plain name, else one
/// derived from the address.
#[must_use]
pub fn invitee_name(display: Option<&str>, email: Option<&str>) -> Option<String> {
    display
        .and_then(|d| {
            // Apple Calendar shows an invitee with no display name as their
            // address; that is still a name to be had.
            if d.contains('@') {
                name_from_email(d)
            } else {
                clean_name(d)
            }
        })
        .or_else(|| email.and_then(name_from_email))
}

/// Whether `word` is a short name heard shorter: "Ky" for "Kai". Only for
/// names of three letters or fewer, which string similarity cannot reach;
/// the heard word must be shorter than the name (so "Kia" is somebody else),
/// start with the same letter, and keep the same consonants.
fn short_name_heard(word: &str, name: &str) -> bool {
    let consonants = |w: &str| -> String {
        sounded(w)
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphabetic() && !"aeiouy".contains(*c))
            .collect()
    };
    let (word_len, name_len) = (word.chars().count(), name.chars().count());
    name_len <= 3
        && (2..name_len).contains(&word_len)
        && word.chars().next().map(|c| c.to_ascii_lowercase())
            == name.chars().next().map(|c| c.to_ascii_lowercase())
        && !consonants(name).is_empty()
        && consonants(word) == consonants(name)
}

/// `word` as it sounds for matching: "ph" is "f" ("Raphael" → "Rafael").
fn sounded(word: &str) -> String {
    word.replace("ph", "f")
        .replace("Ph", "F")
        .replace("PH", "F")
}

fn first_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// Capitalised words in `texts` that are near-misses of an invitee's first
/// name, as corrections to that name. Each heard spelling appears once.
#[must_use]
pub fn corrections_for(texts: &[String], names: &[String]) -> Vec<NameCorrection> {
    let firsts: Vec<String> = names.iter().map(|n| first_name(n).to_owned()).collect();
    let known: HashSet<String> = names
        .iter()
        .flat_map(|n| n.split_whitespace())
        .map(str::to_lowercase)
        .collect();
    let profile = Profile {
        corrections: firsts
            .iter()
            .map(|word| Correction {
                word: word.clone(),
                replacement: None,
                matching_threshold: Some(NAME_THRESHOLD),
            })
            .collect(),
        ..Profile::default()
    };
    let matcher = VocabularyMatcher::new(&profile);
    if matcher.is_empty() {
        return Vec::new();
    }
    // A word the transcript also uses in lower case is an ordinary word,
    // whatever its capitalisation at the start of a sentence: names are
    // always capitalised.
    let lower: HashSet<&str> = texts
        .iter()
        .flat_map(|t| t.split(|c: char| !c.is_alphabetic()))
        .filter(|w| w.chars().next().is_some_and(char::is_lowercase))
        .collect();
    let mut found: Vec<NameCorrection> = Vec::new();
    let mut seen = HashSet::new();
    for text in texts {
        for word in text.split(|c: char| !(c.is_alphabetic() || c == '\'' || c == '’')) {
            let word = word.trim_matches(['\'', '’']);
            // "Rafael's goal": the name is "Rafael".
            let word = word
                .strip_suffix("'s")
                .or_else(|| word.strip_suffix("’s"))
                .unwrap_or(word);
            let capitalised = word.chars().next().is_some_and(char::is_uppercase)
                && word.chars().skip(1).any(char::is_lowercase);
            if !capitalised
                || known.contains(&word.to_lowercase())
                || lower.contains(word.to_lowercase().as_str())
                || seen.contains(word)
            {
                continue;
            }
            let mut fixed = matcher.apply(word);
            if fixed == word {
                // Spelled differently, sounds the same: "Raphael" for "Rafiya"
                // reads as "Rafael" to the matcher.
                let spoken = sounded(word);
                if spoken != word {
                    fixed = matcher.apply(&spoken);
                }
            }
            if fixed == word
                && let Some(short) = firsts.iter().find(|f| short_name_heard(word, f))
            {
                fixed.clone_from(short);
            }
            if fixed != word
                && let Some(to) = firsts.iter().find(|f| f.eq_ignore_ascii_case(&fixed))
            {
                seen.insert(word.to_owned());
                found.push(NameCorrection {
                    from: word.to_owned(),
                    to: to.clone(),
                });
            }
        }
    }
    found
}

fn read_all(db: &Db) -> BTreeMap<String, MeetingInvitees> {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn write_all(db: &mut Db, all: &BTreeMap<String, MeetingInvitees>) -> Result<(), String> {
    let json = serde_json::to_string(all).map_err(|e| e.to_string())?;
    db.put_setting(SETTINGS_KEY, &json)
        .map_err(|e| e.to_string())
}

/// A meeting's invitees; empty when none were recorded.
#[must_use]
pub fn read(db: &Db, meeting_id: &str) -> MeetingInvitees {
    read_all(db).remove(meeting_id).unwrap_or_default()
}

/// Keep `names` as `meeting_id`'s invitees, replacing any before. Corrections
/// are recomputed by [`refresh_corrections`].
///
/// # Errors
/// The library write failed.
pub fn store_names(db: &mut Db, meeting_id: &str, names: &[String]) -> Result<(), String> {
    let mut all = read_all(db);
    let mut unique = Vec::new();
    for name in names.iter().filter_map(|n| clean_name(n)) {
        if !unique
            .iter()
            .any(|u: &String| u.eq_ignore_ascii_case(&name))
        {
            unique.push(name);
        }
    }
    unique.truncate(MAX_INVITEES);
    all.insert(
        meeting_id.to_owned(),
        MeetingInvitees {
            names: unique,
            corrections: Vec::new(),
        },
    );
    write_all(db, &all)
}

/// Find corrections for `meeting_id` from its stored transcript, keep them,
/// and apply them to the current summary.
///
/// # Errors
/// The library could not be read or written.
pub fn refresh_corrections(db: &mut Db, meeting_id: &str) -> Result<Vec<NameCorrection>, String> {
    let mut all = read_all(db);
    let Some(entry) = all.get_mut(meeting_id) else {
        return Ok(Vec::new());
    };
    if entry.names.is_empty() {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = crate::summarize::stored_segments(db, meeting_id)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.text)
        .collect();
    entry.corrections = corrections_for(&texts, &entry.names);
    let found = entry.corrections.clone();
    write_all(db, &all)?;
    if !found.is_empty() {
        crate::documents::add_corrections_to_document(db, meeting_id, &found)?;
        let names = crate::documents::name_corrections_for(db, meeting_id);
        crate::documents::correct_current_summary(db, meeting_id, &names)?;
    }
    Ok(found)
}

/// Invitee names to give the summary engine as spellings, when allowed.
#[must_use]
pub fn for_summary(db: &Db, meeting_id: &str) -> Vec<String> {
    summary_inputs(db, meeting_id).0
}

pub(crate) fn summary_inputs(db: &Db, meeting_id: &str) -> (Vec<String>, Option<String>) {
    match enabled(db) {
        Ok(None | Some(true)) => (read(db, meeting_id).names, None),
        Ok(Some(false)) => (Vec::new(), None),
        Err(error) => {
            crate::journal::record(&format!("summary: {error}"));
            (Vec::new(), Some(error))
        }
    }
}

/// The saved choice about including invitee names in summary requests.
///
/// # Errors
/// The saved choice could not be read or is invalid.
pub fn enabled(db: &Db) -> Result<Option<bool>, String> {
    let repair = "Invitee names are withheld. Save your choice again in Settings › Calendars. Use Save Off to keep names withheld.";
    match db.get_setting(ENABLED_KEY) {
        Ok(None) => Ok(None),
        Ok(Some(value)) => match value.trim() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(format!(
                "The saved invitee-name setting is invalid. {repair}"
            )),
        },
        Err(_) => Err(format!(
            "Could not read the saved invitee-name setting. {repair}"
        )),
    }
}

/// Turn sending invitee names to the summary engine on or off.
///
/// # Errors
/// The library write failed.
pub fn set_enabled(db: &mut Db, on: bool) -> Result<(), String> {
    db.put_setting(ENABLED_KEY, if on { "true" } else { "false" })
        .map_err(|e| e.to_string())
}

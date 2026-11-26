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

/// Whether invitee names are sent to the summary engine. On unless turned
/// off in Settings › Calendars.
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

fn decode_all(value: Option<&str>) -> Result<BTreeMap<String, MeetingInvitees>, String> {
    value.map_or_else(
        || Ok(BTreeMap::new()),
        |json| serde_json::from_str(json).map_err(|_| "Invalid saved invitee map".to_owned()),
    )
}

fn encode_all(all: &BTreeMap<String, MeetingInvitees>) -> Result<String, String> {
    serde_json::to_string(all).map_err(|e| e.to_string())
}

fn update_error(error: String) -> String {
    format!(
        "Could not update saved invitees: {error}. Retry when the library is available. \
         If this persists, check the library or restore a healthy backup."
    )
}

/// A meeting's invitees, or empty when no readable names were recorded.
#[must_use]
pub fn read(db: &Db, meeting_id: &str) -> MeetingInvitees {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .and_then(|value| decode_all(value.as_deref()).ok())
        .and_then(|mut all| all.remove(meeting_id))
        .unwrap_or_default()
}

fn normalized_names(names: &[String]) -> Vec<String> {
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
    unique
}

/// Keep `names` as `meeting_id`'s invitees, replacing any before. Corrections
/// are recomputed by [`refresh_corrections`].
///
/// # Errors
/// The library could not be read or written.
pub fn store_names(db: &mut Db, meeting_id: &str, names: &[String]) -> Result<(), String> {
    let unique = normalized_names(names);
    db.update_setting(SETTINGS_KEY, |_, current| {
        let mut all = decode_all(current.as_deref())?;
        all.insert(
            meeting_id.to_owned(),
            MeetingInvitees {
                names: unique,
                corrections: Vec::new(),
            },
        );
        Ok((Some(encode_all(&all)?), ()))
    })
    .map_err(update_error)
}

/// Find corrections for `meeting_id` from its stored transcript, keep them,
/// and apply them to the current summary.
///
/// # Errors
/// The library could not be read or written.
pub fn refresh_corrections(db: &mut Db, meeting_id: &str) -> Result<Vec<NameCorrection>, String> {
    refresh_with_invitees(db, meeting_id, &[]).map(|(_, corrections)| corrections)
}

/// Merge names found in a calendar repair and refresh corrections together.
pub(crate) fn refresh_with_invitees(
    db: &mut Db,
    meeting_id: &str,
    additional_names: &[String],
) -> Result<(usize, Vec<NameCorrection>), String> {
    let (count, found, replacement) =
        prepare_refresh(db, meeting_id, additional_names).map_err(update_error)?;
    let Some(replacement) = replacement else {
        return Ok((count, found));
    };
    save_refresh(db, meeting_id, replacement).map_err(update_error)?;
    if !found.is_empty() {
        crate::documents::add_corrections_to_document(db, meeting_id, &found)?;
        let names = crate::documents::name_corrections_for(db, meeting_id);
        crate::documents::correct_current_summary(db, meeting_id, &names)?;
    }
    Ok((count, found))
}

struct PreparedRefresh {
    previous: Option<MeetingInvitees>,
    replacement: MeetingInvitees,
    input_epoch: String,
}

type Refresh = (usize, Vec<NameCorrection>, Option<PreparedRefresh>);

fn prepare_refresh(
    db: &Db,
    meeting_id: &str,
    additional_names: &[String],
) -> Result<Refresh, String> {
    let current = db
        .get_setting(SETTINGS_KEY)
        .map_err(|error| error.to_string())?;
    let previous = decode_all(current.as_deref())?.remove(meeting_id);
    let mut entry = previous.clone().unwrap_or_default();
    let old_count = entry.names.len();
    for name in additional_names.iter().filter_map(|name| clean_name(name)) {
        if !entry
            .names
            .iter()
            .any(|old| old.eq_ignore_ascii_case(&name))
        {
            entry.names.push(name);
        }
    }
    if entry.names.len() != old_count {
        entry.names = normalized_names(&entry.names);
    }
    let count = entry.names.len();
    if count == 0 {
        return Ok((0, Vec::new(), None));
    }

    let input_epoch = db
        .meeting_input_epoch(meeting_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Meeting input generation is unavailable".to_owned())?;
    let primary = db
        .primary_transcript_id(meeting_id)
        .map_err(|error| format!("Could not read the primary transcript: {error}"))?;
    let found = if let Some(transcript_id) = primary {
        let texts: Vec<String> = crate::summarize::load_segments(db, meeting_id, &transcript_id)
            .map_err(|error| format!("Could not read transcript segments: {error}"))?
            .into_iter()
            .map(|segment| segment.text)
            .collect();
        entry.corrections = corrections_for(&texts, &entry.names);
        entry.corrections.clone()
    } else if previous
        .as_ref()
        .is_some_and(|old| old.names == entry.names)
    {
        return Ok((count, Vec::new(), None));
    } else {
        Vec::new()
    };

    let update = PreparedRefresh {
        previous,
        replacement: entry,
        input_epoch,
    };
    Ok((count, found, Some(update)))
}

fn save_refresh(db: &mut Db, meeting_id: &str, update: PreparedRefresh) -> Result<(), String> {
    db.update_setting(SETTINGS_KEY, |db, current| {
        let mut all = decode_all(current.as_deref())?;
        let input_epoch = db
            .meeting_input_epoch(meeting_id)
            .map_err(|error| error.to_string())?;
        if all.get(meeting_id) != update.previous.as_ref()
            || input_epoch.as_ref() != Some(&update.input_epoch)
        {
            return Err("Saved names or meeting inputs changed during refresh".to_owned());
        }
        all.insert(meeting_id.to_owned(), update.replacement);
        Ok((Some(encode_all(&all)?), ()))
    })
}

/// Invitee names to give the summary engine as spellings, when allowed.
#[must_use]
pub fn for_summary(db: &Db, meeting_id: &str) -> Vec<String> {
    if enabled(db) {
        read(db, meeting_id).names
    } else {
        Vec::new()
    }
}

/// Whether invitee names go to the summary engine.
#[must_use]
pub fn enabled(db: &Db) -> bool {
    db.get_setting(ENABLED_KEY)
        .ok()
        .flatten()
        .is_none_or(|v| v.trim() != "false")
}

/// Turn sending invitee names to the summary engine on or off.
///
/// # Errors
/// The library write failed.
pub fn set_enabled(db: &mut Db, on: bool) -> Result<(), String> {
    db.put_setting(ENABLED_KEY, if on { "true" } else { "false" })
        .map_err(|e| e.to_string())
}

#[test]
fn a_refresh_cannot_overwrite_newer_names_or_use_a_changed_transcript() {
    for change_map in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("library.db");
        let key = fotw_store::DbKey::from_bytes([7; 32]);
        let mut db = Db::open(&path, &key).unwrap();
        let mut writer = Db::open(&path, &key).unwrap();
        let id = db
            .meetings()
            .create(fotw_store::NewMeeting::new("fixture", "UTC"))
            .unwrap();
        let transcript = db
            .meetings()
            .create_transcript(&id, "fixture", "fixture", true)
            .unwrap();
        db.meetings()
            .append_segments(
                &transcript,
                &[fotw_store::NewSegment::new(0, 0, 100, "Rafael is here.")],
            )
            .unwrap();
        store_names(&mut db, &id, &["Rafiya".to_owned()]).unwrap();
        let (_, found, update) = prepare_refresh(&db, &id, &[]).unwrap();
        assert_eq!(found.len(), 1);
        let update = update.unwrap();

        if change_map {
            store_names(&mut writer, &id, &["Kai".to_owned()]).unwrap();
        } else {
            writer
                .conn()
                .execute(
                    "UPDATE segments SET text = 'The meeting is ready.' WHERE transcript_id = ?1",
                    [&transcript],
                )
                .unwrap();
        }
        let before = writer.get_setting(SETTINGS_KEY).unwrap();
        let error = save_refresh(&mut db, &id, update).unwrap_err();
        assert!(error.contains("changed during refresh"));
        assert_eq!(db.get_setting(SETTINGS_KEY).unwrap(), before);
    }
}

#[test]
fn a_calendar_refresh_preserves_a_concurrent_update_to_another_meeting() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("library.db");
    let key = fotw_store::DbKey::from_bytes([7; 32]);
    let mut db = Db::open(&path, &key).unwrap();
    let mut writer = Db::open(&path, &key).unwrap();
    let meeting = db
        .meetings()
        .create(fotw_store::NewMeeting::new("First fixture", "UTC"))
        .unwrap();
    let other = db
        .meetings()
        .create(fotw_store::NewMeeting::new("Second fixture", "UTC"))
        .unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&meeting, "fixture", "fixture", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &transcript,
            &[fotw_store::NewSegment::new(0, 0, 100, "Rafael is here.")],
        )
        .unwrap();
    store_names(&mut db, &other, &["Kai".to_owned()]).unwrap();

    let (count, found, update) = prepare_refresh(&db, &meeting, &["Rafiya".to_owned()]).unwrap();
    assert_eq!(count, 1);
    assert_eq!(found.len(), 1);
    let update = update.unwrap();
    store_names(&mut writer, &other, &["Morgan".to_owned()]).unwrap();

    save_refresh(&mut db, &meeting, update).unwrap();
    let saved = read(&writer, &meeting);
    assert_eq!(saved.names, ["Rafiya"]);
    assert_eq!(saved.corrections, found);
    assert_eq!(read(&writer, &other).names, ["Morgan"]);
}

#[test]
fn calendar_repair_normalizes_existing_names_when_adding_a_name() {
    let root = tempfile::tempdir().unwrap();
    let key = fotw_store::DbKey::from_bytes([7; 32]);
    let mut db = Db::open(root.path().join("library.db"), &key).unwrap();
    let meeting = db
        .meetings()
        .create(fotw_store::NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let stored = serde_json::json!({ &meeting: { "names": ["  Kit  "] } });
    db.put_setting(SETTINGS_KEY, &stored.to_string()).unwrap();

    refresh_with_invitees(&mut db, &meeting, &["Kai".to_owned()]).unwrap();

    assert_eq!(read(&db, &meeting).names, ["Kit", "Kai"]);
}

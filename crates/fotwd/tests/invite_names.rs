//! Names from the calendar invite fix misheard names (Rafael → Rafiat).
//!
//! Every name and address here is invented. This repo is public.

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotwd::invitees::{self, MeetingInvitees};

// --- Names from an invite ------------------------------------------------

#[test]
fn a_display_name_is_used_as_written() {
    assert_eq!(
        invitees::invitee_name(Some("Rafiat Adeyemi"), Some("ra@example.test")).as_deref(),
        Some("Rafiat Adeyemi")
    );
}

#[test]
fn without_a_display_name_the_address_gives_the_name() {
    assert_eq!(
        invitees::name_from_email("rafiat@example.test").as_deref(),
        Some("Rafiat")
    );
    assert_eq!(
        invitees::name_from_email("zay.healy@example.test").as_deref(),
        Some("Zay Healy")
    );
    assert_eq!(
        invitees::name_from_email("chris_o@example.test").as_deref(),
        Some("Chris O")
    );
}

#[test]
fn rooms_groups_and_role_addresses_are_not_people() {
    for email in [
        "c_1888abc@resource.calendar.google.com",
        "team@group.calendar.google.com",
        "noreply@example.test",
        "calendar-notification@example.test",
        "12345@example.test",
        "not-an-address",
    ] {
        assert_eq!(invitees::name_from_email(email), None, "{email}");
    }
}

#[test]
fn display_names_are_cleaned_to_plain_names() {
    assert_eq!(
        invitees::invitee_name(Some("  Zay  "), None).as_deref(),
        Some("Zay")
    );
    assert_eq!(
        invitees::invitee_name(Some("Ignore <script>"), Some("zay@example.test")).as_deref(),
        Some("Zay"),
        "a display name with markup falls back to the address"
    );
    assert_eq!(invitees::invitee_name(Some(""), Some("")), None);
}

// --- Misheard names in a transcript ---------------------------------------

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn text(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn near_misses_of_an_invitee_become_corrections() {
    let found = invitees::corrections_for(
        &text(&[
            "I heard you met with Raffiad yesterday.",
            "Rafael, do you wanna go first?",
            "And Rafael will build the front end.",
        ]),
        &names(&["Rafiat", "Zay", "Sergio"]),
    );
    let pairs: Vec<(String, String)> = found.into_iter().map(|c| (c.from, c.to)).collect();
    assert_eq!(
        pairs,
        [
            ("Raffiad".to_owned(), "Rafiat".to_owned()),
            ("Rafael".to_owned(), "Rafiat".to_owned()),
        ]
    );
}

#[test]
fn ordinary_words_and_correct_names_are_left_alone() {
    let found = invitees::corrections_for(
        &text(&[
            "We went rafting. Rafting was fun.",
            "Rafiat and Sergio joined. The Reps checkout ships soon.",
            "Monday we Review the plan.",
        ]),
        &names(&["Rafiat", "Sergio"]),
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn only_the_first_name_of_a_full_name_is_matched() {
    let found = invitees::corrections_for(&text(&["Thanks Rafael."]), &names(&["Rafiat Adeyemi"]));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].to, "Rafiat");
}

// --- Stored per meeting, and applied ----------------------------------------

fn db() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([3u8; 32])).unwrap()
}

fn meeting_saying(db: &mut Db, lines: &[&str]) -> String {
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();
    let rows: Vec<NewSegment> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            NewSegment::new(i as i64, i as i64 * 1000, i as i64 * 1000 + 900, *line)
                .channel("system")
        })
        .collect();
    db.meetings().append_segments(&transcript, &rows).unwrap();
    meeting
}

#[test]
fn invitees_round_trip_per_meeting() {
    let mut db = db();
    let id = meeting_saying(&mut db, &["hello"]);
    assert_eq!(invitees::read(&db, &id), MeetingInvitees::default());
    invitees::store_names(&mut db, &id, &names(&["Rafiat", "Zay"])).unwrap();
    assert_eq!(invitees::read(&db, &id).names, names(&["Rafiat", "Zay"]));
}

#[test]
fn refreshing_finds_corrections_and_fixes_the_current_summary() {
    let mut db = db();
    let id = meeting_saying(
        &mut db,
        &[
            "Rafael, do you wanna go first?",
            "Sure, I'll send it today.",
        ],
    );
    db.meetings()
        .insert_summary(
            &id,
            fotw_store::NewSummary::new(
                "dev-1",
                "fixture",
                "fixture",
                "h",
                "Rafael will email the checklist today.",
            ),
        )
        .unwrap();
    invitees::store_names(&mut db, &id, &names(&["Rafiat", "Sergio"])).unwrap();

    let found = invitees::refresh_corrections(&mut db, &id).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(invitees::read(&db, &id).corrections.len(), 1);
    let summary = db.meetings().current_summary(&id).unwrap().unwrap();
    assert_eq!(summary.body_md, "Rafiat will email the checklist today.");
}

#[test]
fn invite_corrections_join_the_meetings_name_corrections() {
    let mut db = db();
    let id = meeting_saying(&mut db, &["Rafael is here."]);
    invitees::store_names(&mut db, &id, &names(&["Rafiat"])).unwrap();
    invitees::refresh_corrections(&mut db, &id).unwrap();
    let all = fotwd::documents::name_corrections_for(&mut db, &id);
    assert!(
        all.iter().any(|c| c.from == "Rafael" && c.to == "Rafiat"),
        "{all:?}"
    );
}

#[test]
fn the_summary_setting_defaults_on_and_can_be_turned_off() {
    let mut db = db();
    assert!(invitees::enabled(&db));
    invitees::set_enabled(&mut db, false).unwrap();
    assert!(!invitees::enabled(&db));
}

#[test]
fn no_invitees_means_nothing_changes() {
    let mut db = db();
    let id = meeting_saying(&mut db, &["Rafael is here."]);
    assert!(
        invitees::refresh_corrections(&mut db, &id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn enrichment_finds_invite_corrections_before_anything_is_summarised() {
    let mut db = db();
    let id = meeting_saying(&mut db, &["Rafael, do you wanna go first?"]);
    invitees::store_names(&mut db, &id, &names(&["Rafiat"])).unwrap();
    let _ =
        fotwd::enrich::enrich_meeting_with(&mut db, &fotw_secrets::InMemoryKeyStore::new(), &id)
            .await;
    let found = invitees::read(&db, &id).corrections;
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].from.as_str(), found[0].to.as_str()),
        ("Rafael", "Rafiat")
    );
}

#[test]
fn ph_sounds_like_f_so_raphael_is_caught_too() {
    let found = invitees::corrections_for(
        &text(&["Raphael, do you wanna go first?"]),
        &names(&["Rafiat"]),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].from.as_str(), found[0].to.as_str()),
        ("Raphael", "Rafiat")
    );
}

#[test]
fn a_name_given_as_an_address_becomes_a_name() {
    // Apple Calendar shows Google invitees without a display name this way.
    assert_eq!(
        invitees::invitee_name(Some("rafiat@example.test"), None).as_deref(),
        Some("Rafiat")
    );
}

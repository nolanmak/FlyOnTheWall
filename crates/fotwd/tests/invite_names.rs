//! Names from the calendar invite fix misheard names (Rafael → Rafiya).
//!
//! Every name and address here is invented. This repo is public.

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotwd::invitees::{self, MeetingInvitees};

// --- Names from an invite ------------------------------------------------

#[test]
fn a_display_name_is_used_as_written() {
    assert_eq!(
        invitees::invitee_name(Some("Rafiya Lindqvist"), Some("ra@example.test")).as_deref(),
        Some("Rafiya Lindqvist")
    );
}

#[test]
fn without_a_display_name_the_address_gives_the_name() {
    assert_eq!(
        invitees::name_from_email("rafiya@example.test").as_deref(),
        Some("Rafiya")
    );
    assert_eq!(
        invitees::name_from_email("kit.marlow@example.test").as_deref(),
        Some("Kit Marlow")
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
        invitees::invitee_name(Some("  Kai  "), None).as_deref(),
        Some("Kai")
    );
    assert_eq!(
        invitees::invitee_name(Some("Ignore <script>"), Some("kai@example.test")).as_deref(),
        Some("Kai"),
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
        &names(&["Rafiya", "Kai", "Jonas"]),
    );
    let pairs: Vec<(String, String)> = found.into_iter().map(|c| (c.from, c.to)).collect();
    assert_eq!(
        pairs,
        [
            ("Raffiad".to_owned(), "Rafiya".to_owned()),
            ("Rafael".to_owned(), "Rafiya".to_owned()),
        ]
    );
}

#[test]
fn ordinary_words_and_correct_names_are_left_alone() {
    let found = invitees::corrections_for(
        &text(&[
            "We went rafting. Rafting was fun.",
            "Rafiya and Jonas joined. The checkout ships soon.",
            "Monday we Review the plan.",
        ]),
        &names(&["Rafiya", "Jonas"]),
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn only_the_first_name_of_a_full_name_is_matched() {
    let found =
        invitees::corrections_for(&text(&["Thanks Rafael."]), &names(&["Rafiya Lindqvist"]));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].to, "Rafiya");
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
    invitees::store_names(&mut db, &id, &names(&["Rafiya", "Kai"])).unwrap();
    assert_eq!(invitees::read(&db, &id).names, names(&["Rafiya", "Kai"]));
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
    invitees::store_names(&mut db, &id, &names(&["Rafiya", "Jonas"])).unwrap();

    let found = invitees::refresh_corrections(&mut db, &id).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(invitees::read(&db, &id).corrections.len(), 1);
    let summary = db.meetings().current_summary(&id).unwrap().unwrap();
    assert_eq!(summary.body_md, "Rafiya will email the checklist today.");
}

#[test]
fn invite_corrections_join_the_meetings_name_corrections() {
    let mut db = db();
    let id = meeting_saying(&mut db, &["Rafael is here."]);
    invitees::store_names(&mut db, &id, &names(&["Rafiya"])).unwrap();
    invitees::refresh_corrections(&mut db, &id).unwrap();
    let all = fotwd::documents::name_corrections_for(&mut db, &id);
    assert!(
        all.iter().any(|c| c.from == "Rafael" && c.to == "Rafiya"),
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
    invitees::store_names(&mut db, &id, &names(&["Rafiya"])).unwrap();
    let _ =
        fotwd::enrich::enrich_meeting_with(&mut db, &fotw_secrets::InMemoryKeyStore::new(), &id)
            .await;
    let found = invitees::read(&db, &id).corrections;
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].from.as_str(), found[0].to.as_str()),
        ("Rafael", "Rafiya")
    );
}

#[test]
fn ph_sounds_like_f_so_raphael_is_caught_too() {
    let found = invitees::corrections_for(
        &text(&["Raphael, do you wanna go first?"]),
        &names(&["Rafiya"]),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].from.as_str(), found[0].to.as_str()),
        ("Raphael", "Rafiya")
    );
}

#[test]
fn a_name_given_as_an_address_becomes_a_name() {
    // Apple Calendar shows Google invitees without a display name this way.
    assert_eq!(
        invitees::invitee_name(Some("rafiya@example.test"), None).as_deref(),
        Some("Rafiya")
    );
}

#[test]
fn a_possessive_is_matched_on_its_name() {
    let found = invitees::corrections_for(
        &text(&["Rafael's goal of x, y, z was really good."]),
        &names(&["Rafiya"]),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].from.as_str(), found[0].to.as_str()),
        ("Rafael", "Rafiya")
    );
    // And the correction then reads naturally in place.
    assert_eq!(
        fotw_web::documents::corrected_names("Rafael's goal", &found),
        "Rafiya's goal"
    );
}

#[test]
fn a_short_name_heard_shorter_is_caught_by_its_consonants() {
    let found = invitees::corrections_for(
        &text(&["So Ky was traveling this week."]),
        &names(&["Kai", "Matt"]),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].from.as_str(), found[0].to.as_str()),
        ("Ky", "Kai")
    );
}

#[test]
fn a_different_short_name_of_the_same_length_is_left_alone() {
    // Kia is somebody else, not a mishearing of Kai.
    let found = invitees::corrections_for(
        &text(&["Kia joined late.", "Al and Ali both spoke."]),
        &names(&["Kai", "Al"]),
    );
    assert!(found.is_empty(), "{found:?}");
}

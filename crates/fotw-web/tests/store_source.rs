//! [`StoreSource`] against a real database.
//!
//! # Why this file had to exist
//!
//! The rest of the suite drives the API through `MemorySource`, a fixture that
//! hands back exactly the `MeetingDetail` it was constructed with. That is the
//! right tool for testing ingress, routing and serialisation — but it means an
//! assertion like "the response carries a speaker label" passes without the
//! translation from SQLite ever running. The API shipped for weeks returning
//! `{idx, text}` and nothing else, with a green suite, because *every* test of
//! the detail payload went through the fixture.
//!
//! Proven, not assumed: mutating `StoreSource::detail` to drop the speaker,
//! zero the offset, or discard the note leaves the `MemorySource`-backed tests
//! passing and fails these. That difference is the entire point of the file.

#![cfg(feature = "store")]

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotw_web::StoreSource;
use fotw_web::source::MeetingSource;

const SPEAKER: &str = "S1";
const NOTE: &str = "ask about the rebinding guard";
const WORDS: &str = "the aggregate device disappeared when the dock woke up";

fn seeded() -> (StoreSource, String) {
    let dir = std::env::temp_dir().join(format!(
        "fotw-web-src-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut db = Db::open(dir.join("db.sqlite3"), &DbKey::from_bytes([0x33; 32])).unwrap();

    let id = db
        .meetings()
        .create({
            let mut m = NewMeeting::new("test", "UTC");
            m.title = "Device changes".to_owned();
            m.started_at_ms = 1_754_900_000_000;
            m
        })
        .unwrap();

    let transcript = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();

    db.meetings()
        .append_segments(
            &transcript,
            &[{
                let mut s = NewSegment::new(0, 45_000, 49_500, WORDS);
                s.speaker_label = Some(SPEAKER.to_owned());
                s.confidence = Some(0.91);
                s
            }],
        )
        .unwrap();

    db.meetings().upsert_note(&id, NOTE, &[]).unwrap();

    (StoreSource::new(db), id)
}

#[test]
fn the_speaker_label_survives_the_round_trip_from_sqlite() {
    let (source, id) = seeded();
    let detail = source.detail(&id).unwrap().expect("meeting exists");

    assert_eq!(detail.segments.len(), 1);
    assert_eq!(
        detail.segments[0].speaker.as_deref(),
        Some(SPEAKER),
        "the column is written and was never read back"
    );
}

#[test]
fn the_segment_offset_survives_the_round_trip_from_sqlite() {
    let (source, id) = seeded();
    let detail = source.detail(&id).unwrap().expect("meeting exists");

    // A specific non-zero value: asserting `>= 0` would pass against a
    // hard-coded zero, which is exactly what the old code returned.
    assert_eq!(detail.segments[0].start_ms, 45_000);
}

#[test]
fn the_note_survives_the_round_trip_from_sqlite() {
    let (source, id) = seeded();
    let detail = source.detail(&id).unwrap().expect("meeting exists");

    assert_eq!(detail.note_md.as_deref(), Some(NOTE));
}

#[test]
fn the_words_still_come_back_too() {
    let (source, id) = seeded();
    let detail = source.detail(&id).unwrap().expect("meeting exists");
    assert_eq!(detail.segments[0].text, WORDS);
}

/// The premise #78 rests on: the meeting the daemon announces is one the very
/// next `/api/meetings` can already see.
///
/// The suite's first `list()` test at all. Every existing test here drives
/// `detail()`, so the list view — the app's home screen, and the thing a
/// `meeting_ready` frame tells the client to re-fetch — went through the same
/// `MemorySource` fixture that hands back whatever it was constructed with.
/// Seeded through the exact sequence `fotwd::persist::persist_session` runs,
/// in its order, so this fails if that order ever stops leaving a queryable
/// row behind.
#[test]
fn a_persisted_session_appears_in_list() {
    let dir = std::env::temp_dir().join(format!("fotw-web-src-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = Db::open(dir.join("db.sqlite3"), &DbKey::from_bytes([0x55; 32])).unwrap();

    let id = db
        .meetings()
        .create({
            let mut m = NewMeeting::new("test", "UTC");
            m.title = "Untitled recording — 1754900000".to_owned();
            m.started_at_ms = 1_754_900_000_000;
            m
        })
        .unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(&transcript, &[NewSegment::new(0, 0, 1_000, WORDS)])
        .unwrap();
    db.meetings().finish(&id, 1_754_900_060_000).unwrap();
    // The state move is a separate call from `finish`, and the one that
    // matters here: without it the row lists as still `recording`.
    db.meetings().set_state(&id, "ready").unwrap();

    let source = StoreSource::new(db);
    let rows = source.list(50, 0).unwrap();
    let found = rows
        .iter()
        .find(|r| r.id == id)
        .expect("the meeting the daemon just announced must be listable");
    assert_eq!(
        found.state, "ready",
        "a meeting announced as ready must not list as still recording"
    );
    assert_eq!(found.started_at_ms, 1_754_900_000_000);

    // Newest first (`repo.rs`'s `started_at_ms DESC`): a meeting that just
    // ended is the one the user is looking at, and a refresh that buried it
    // page-deep would be no refresh at all.
    assert_eq!(rows[0].id, id, "the newest meeting leads the list");
}

/// A meeting with no note is `None`, not an empty string — and, more to the
/// point, asking for the note of a meeting that has none is not an error.
#[test]
fn a_meeting_with_no_note_is_not_an_error() {
    let (source, _) = seeded();
    let dir = std::env::temp_dir().join(format!("fotw-web-src-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = Db::open(dir.join("db.sqlite3"), &DbKey::from_bytes([0x44; 32])).unwrap();
    let id = db
        .meetings()
        .create({
            let mut m = NewMeeting::new("test", "UTC");
            m.title = "Silent".to_owned();
            m
        })
        .unwrap();
    drop(source);

    let source = StoreSource::new(db);
    let detail = source.detail(&id).unwrap().expect("meeting exists");
    assert!(detail.note_md.is_none());
    assert!(detail.segments.is_empty(), "no transcript is not an error");
}

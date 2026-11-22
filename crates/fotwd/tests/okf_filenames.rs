//! Export file names for the local OKF bundle (`fotwd export-okf`).
//!
//! A meeting's file name ends in a fragment of its id. Store ids are UUIDv7,
//! whose first 8 hex digits are the top of a millisecond timestamp and change
//! only about every 65 seconds, so two meetings created inside one such window
//! share that fragment. When they also share a title and a UTC start date,
//! for example a recording stopped and started again inside one calendar
//! event, both of which take the event's title, the short name alone cannot
//! tell them apart.
//!
//! The meetings here are inserted with fixed ids, so a test does not depend on
//! how fast it runs. Synthetic data only.

use std::path::Path;

use fotw_store::{Db, DbKey};
use fotwd::okf::{export_bundle, transcript_filename};

/// 2025-08-21 00:00 UTC.
const T0: i64 = 1_755_734_400_000;
const HOUR: i64 = 3_600_000;

/// Two ids minted inside one ~65 s window: the same first 8 hex digits.
const EARLIER: &str = "01a2044d-1c00-7000-8000-00000000000a";
const LATER: &str = "01a2044d-9f00-7000-8000-00000000000b";
/// An id from the next window.
const NEXT_WINDOW: &str = "01a2044e-0100-7000-8000-00000000000c";

fn library() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([0x01; 32])).unwrap()
}

/// A ready meeting with one transcript segment, under a fixed id.
fn meeting(db: &mut Db, id: &str, title: &str, started_at_ms: i64, words: &str) {
    db.conn()
        .execute(
            &format!(
                "INSERT INTO meetings (id, title, started_at_ms, tz, state, created_at, updated_at, origin_device_id) \
                 VALUES ('{id}', '{title}', {started_at_ms}, 'UTC', 'ready', {started_at_ms}, {started_at_ms}, 'dev-1')"
            ),
            [],
        )
        .unwrap();
    let tid = db
        .meetings()
        .create_transcript(id, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(&tid, &[fotw_store::NewSegment::new(0, 0, 1_500, words)])
        .unwrap();
}

/// The meeting files in `dir`, sorted; `index.md` and `log.md` left out.
fn meeting_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "index.md" && n != "log.md")
        .collect();
    names.sort();
    names
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

/// The one file in `dir` that holds `words`.
fn file_with(dir: &Path, words: &str) -> String {
    let holding: Vec<String> = meeting_files(dir)
        .into_iter()
        .filter(|n| read(dir, n).contains(words))
        .collect();
    assert_eq!(
        holding.len(),
        1,
        "exactly one file holds {words:?}: {holding:?}"
    );
    holding.into_iter().next().unwrap()
}

#[test]
fn same_titled_meetings_minted_together_are_written_to_two_files() {
    let mut db = library();
    meeting(&mut db, EARLIER, "Standup", T0, "the first meeting");
    meeting(&mut db, LATER, "Standup", T0 + HOUR, "the second meeting");
    assert_eq!(
        transcript_filename(T0, "Standup", EARLIER),
        transcript_filename(T0 + HOUR, "Standup", LATER),
        "precondition: the short names collide"
    );

    let out = tempfile::TempDir::new().unwrap();
    assert_eq!(export_bundle(&mut db, out.path()).unwrap(), 2);

    let files = meeting_files(out.path());
    assert_eq!(files.len(), 2, "one file per meeting: {files:?}");
    let first = file_with(out.path(), "the first meeting");
    let second = file_with(out.path(), "the second meeting");
    assert_ne!(first, second);

    let index = read(out.path(), "index.md");
    let log = read(out.path(), "log.md");
    for name in [&first, &second] {
        assert!(
            index.contains(&format!("(./{name})")),
            "index.md links {name}:\n{index}"
        );
        assert!(
            log.contains(&format!("(./{name})")),
            "log.md links {name}:\n{log}"
        );
    }
}

/// The earlier-minted meeting keeps the name it has always had, and the later
/// one gets a longer one, whichever of the two started first and so is
/// listed and written first.
#[test]
fn the_earlier_minted_meeting_keeps_its_name_whatever_the_export_order() {
    for (earlier_start, later_start) in [(T0, T0 + HOUR), (T0 + HOUR, T0)] {
        let mut db = library();
        meeting(
            &mut db,
            EARLIER,
            "Standup",
            earlier_start,
            "the first meeting",
        );
        meeting(&mut db, LATER, "Standup", later_start, "the second meeting");
        let out = tempfile::TempDir::new().unwrap();
        export_bundle(&mut db, out.path()).unwrap();

        let short = transcript_filename(earlier_start, "Standup", EARLIER);
        assert!(
            read(out.path(), &short).contains("the first meeting"),
            "{short} holds the earlier-minted meeting (starts {earlier_start}, {later_start})"
        );
        let second = file_with(out.path(), "the second meeting");
        assert_ne!(second, short);
        assert!(
            second.starts_with("2025-08-21-standup-01a2044d") && second.ends_with(".md"),
            "{second}"
        );
    }
}

/// An exported meeting's file is not renamed when a colliding meeting turns
/// up later, and a re-run writes the same names again.
#[test]
fn names_are_stable_across_exports_and_a_late_collider_renames_nothing() {
    let mut db = library();
    meeting(&mut db, EARLIER, "Standup", T0, "the first meeting");
    let out = tempfile::TempDir::new().unwrap();
    export_bundle(&mut db, out.path()).unwrap();
    let before = meeting_files(out.path());
    assert_eq!(before, [transcript_filename(T0, "Standup", EARLIER)]);

    meeting(&mut db, LATER, "Standup", T0 + HOUR, "the second meeting");
    export_bundle(&mut db, out.path()).unwrap();
    let after = meeting_files(out.path());
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(file_with(out.path(), "the first meeting"), before[0]);

    export_bundle(&mut db, out.path()).unwrap();
    assert_eq!(
        meeting_files(out.path()),
        after,
        "a re-run writes the same names"
    );
}

/// Meetings whose ids fall in different windows already had distinct names,
/// and keep them.
#[test]
fn ids_from_different_windows_keep_their_short_names() {
    let mut db = library();
    meeting(&mut db, EARLIER, "Standup", T0, "the first meeting");
    meeting(
        &mut db,
        NEXT_WINDOW,
        "Standup",
        T0 + HOUR,
        "the next meeting",
    );
    let out = tempfile::TempDir::new().unwrap();
    export_bundle(&mut db, out.path()).unwrap();
    assert_eq!(
        meeting_files(out.path()),
        [
            transcript_filename(T0, "Standup", EARLIER),
            transcript_filename(T0 + HOUR, "Standup", NEXT_WINDOW),
        ]
    );
}

/// Ids that differ only in punctuation or case are kept as they are by an
/// archive import, and must still get a file each.
#[test]
fn imported_ids_differing_only_in_punctuation_get_two_files() {
    let mut db = library();
    meeting(&mut db, "m-1", "Sync", T0, "the hyphenated id");
    meeting(&mut db, "m1", "Sync", T0 + HOUR, "the plain id");
    let out = tempfile::TempDir::new().unwrap();
    assert_eq!(export_bundle(&mut db, out.path()).unwrap(), 2);
    assert_eq!(
        meeting_files(out.path()).len(),
        2,
        "{:?}",
        meeting_files(out.path())
    );
    assert_ne!(
        file_with(out.path(), "the hyphenated id"),
        file_with(out.path(), "the plain id")
    );
}

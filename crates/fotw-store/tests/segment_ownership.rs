//! A transcript's child rows belong to the transcript's meeting, and the
//! database enforces it (#343, the schema half of #336).
//!
//! #338 scoped the readers and the importer. These tests pin the rest: a row
//! that names one meeting and another meeting's transcript is refused at the
//! INSERT, and a library that already holds such rows migrates, repairs them
//! and says how many it found.

use std::collections::BTreeSet;

use fotw_store::{Db, LATEST_SCHEMA_VERSION, SearchQuery};
use rusqlite::{Connection, params};

mod common;
use common::{test_key, tmp_db};

fn raw(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA key = \"x'{}'\";PRAGMA cipher_page_size = 4096;",
        "01".repeat(32)
    ))
    .unwrap();
    conn
}

/// Every migration up to and including 0011: the schema a library is in on
/// the build before this change.
fn apply_v11(conn: &Connection) {
    for sql in [
        include_str!("../src/schema/0001_initial.sql"),
        include_str!("../src/schema/0002_fts.sql"),
        include_str!("../src/schema/0003_enrich_report.sql"),
        include_str!("../src/schema/0004_documents.sql"),
        include_str!("../src/schema/0005_session_notes.sql"),
        include_str!("../src/schema/0006_session_note_links.sql"),
        include_str!("../src/schema/0007_speech_receipts.sql"),
        include_str!("../src/schema/0008_capture_clock_state.sql"),
        include_str!("../src/schema/0009_summary_destination_bootstrap.sql"),
        include_str!("../src/schema/0010_summary_input_generations.sql"),
        include_str!("../src/schema/0011_post_pass.sql"),
    ] {
        conn.execute_batch(sql).unwrap();
    }
    conn.execute_batch("PRAGMA user_version = 11;").unwrap();
}

fn meeting(conn: &Connection, id: &str, title: &str) {
    conn.execute(
        "INSERT INTO meetings (id, title, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
         VALUES (?1, ?2, 0, 'UTC', 'ready', 0, 0, 'd1')",
        params![id, title],
    )
    .unwrap();
}

fn transcript(conn: &Connection, id: &str, meeting_id: &str) {
    conn.execute(
        "INSERT INTO transcripts (id, meeting_id, provider, model, created_at, updated_at, origin_device_id)
         VALUES (?1, ?2, 'deepgram', 'nova-3', 0, 0, 'd1')",
        params![id, meeting_id],
    )
    .unwrap();
}

fn segment(
    conn: &Connection,
    id: &str,
    transcript_id: &str,
    meeting_id: &str,
    idx: i64,
    text: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO segments (id, transcript_id, meeting_id, idx, start_ms, end_ms, channel, text)
         VALUES (?1, ?2, ?3, ?4, 0, 900, 'mic', ?5)",
        params![id, transcript_id, meeting_id, idx, text],
    )
}

fn epoch(
    conn: &Connection,
    id: &str,
    meeting_id: &str,
    transcript_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO speech_epochs (id, meeting_id, transcript_id, session_id, source, epoch,
                                    from_sample, provider, model, capabilities_json)
         VALUES (?1, ?2, ?3, ?1, 'mic', 0, 0, 'local', 'whisper', '{}')",
        params![id, meeting_id, transcript_id],
    )
}

fn receipt(
    conn: &Connection,
    id: &str,
    meeting_id: &str,
    transcript_id: &str,
    epoch_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO speech_receipts (id, meeting_id, transcript_id, epoch_id, from_sample,
                                      through_sample, payload_json)
         VALUES (?1, ?2, ?3, ?4, 0, 10, '{}')",
        params![id, meeting_id, transcript_id, epoch_id],
    )
}

fn summary(
    conn: &Connection,
    id: &str,
    meeting_id: &str,
    transcript_id: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO summaries (id, meeting_id, version, transcript_id, provider, model,
                                prompt_hash, body_md, created_at, origin_device_id)
         VALUES (?1, ?2, 1, ?3, 'mock', 'mock', 'h', 'summary body', 0, 'd1')",
        params![id, meeting_id, transcript_id],
    )
}

fn post_pass_job(
    conn: &Connection,
    meeting_id: &str,
    live: Option<&str>,
    cleaned: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO post_pass_jobs (meeting_id, state, created_at_ms, updated_at_ms, plan_json,
                                     live_transcript_id, cleaned_transcript_id)
         VALUES (?1, 'pending', 0, 0, '{}', ?2, ?3)",
        params![meeting_id, live, cleaned],
    )
}

/// Two meetings with a transcript each, in a current library.
fn two_meetings() -> Db {
    let db = Db::open_in_memory(&test_key()).unwrap();
    let conn = db.conn();
    meeting(conn, "A", "Alpha");
    meeting(conn, "B", "Bravo");
    transcript(conn, "tA", "A");
    transcript(conn, "tB", "B");
    db
}

fn assert_refused(result: rusqlite::Result<usize>, what: &str) {
    let err = result.expect_err(&format!(
        "{what}: a row naming meeting B and meeting A's transcript must be refused"
    ));
    assert!(
        err.to_string().contains("FOREIGN KEY"),
        "{what}: refused for the wrong reason: {err}"
    );
}

#[test]
fn a_segment_naming_another_meetings_transcript_is_refused() {
    let db = two_meetings();
    segment(db.conn(), "s-ok", "tA", "A", 0, "fine").unwrap();
    assert_refused(
        segment(db.conn(), "s-bad", "tA", "B", 1, "leak"),
        "segments",
    );
}

#[test]
fn a_speech_epoch_or_receipt_naming_another_meetings_transcript_is_refused() {
    let db = two_meetings();
    let conn = db.conn();
    epoch(conn, "eA", "A", "tA").unwrap();
    receipt(conn, "rA", "A", "tA", "eA").unwrap();
    assert_refused(epoch(conn, "eX", "B", "tA"), "speech_epochs");
    epoch(conn, "eB", "B", "tB").unwrap();
    assert_refused(receipt(conn, "rX", "B", "tA", "eB"), "speech_receipts");
}

#[test]
fn a_summary_naming_another_meetings_transcript_is_refused() {
    let db = two_meetings();
    summary(db.conn(), "sumA", "A", Some("tA")).unwrap();
    assert_refused(summary(db.conn(), "sumX", "B", Some("tA")), "summaries");
}

#[test]
fn a_post_pass_job_naming_another_meetings_transcript_is_refused() {
    let db = two_meetings();
    assert_refused(post_pass_job(db.conn(), "B", Some("tA"), None), "live id");
    assert_refused(
        post_pass_job(db.conn(), "B", None, Some("tA")),
        "cleaned id",
    );
    post_pass_job(db.conn(), "B", Some("tB"), Some("tB")).unwrap();
}

/// The single-column `ON DELETE` behaviour each table had before survives the
/// combined reference: deleting a transcript still cascades to its segments,
/// epochs and receipts and still clears the nullable pointers.
#[test]
fn deleting_a_transcript_keeps_its_old_cascade_and_set_null_behaviour() {
    let db = two_meetings();
    let conn = db.conn();
    conn.execute(
        "INSERT INTO transcripts (id, meeting_id, provider, model, is_primary, created_at, updated_at, origin_device_id)
         VALUES ('tA2', 'A', 'local', 'whisper', 0, 0, 0, 'd1')",
        [],
    )
    .unwrap();
    segment(conn, "s1", "tA2", "A", 0, "gone with the transcript").unwrap();
    epoch(conn, "e1", "A", "tA2").unwrap();
    receipt(conn, "r1", "A", "tA2", "e1").unwrap();
    summary(conn, "sum1", "A", Some("tA2")).unwrap();
    post_pass_job(conn, "A", Some("tA2"), Some("tA2")).unwrap();

    conn.execute("DELETE FROM transcripts WHERE id = 'tA2'", [])
        .unwrap();

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM segments WHERE id = 's1'"), 0);
    assert_eq!(
        count("SELECT count(*) FROM speech_epochs WHERE id = 'e1'"),
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM speech_receipts WHERE id = 'r1'"),
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM summaries WHERE id = 'sum1' AND transcript_id IS NULL"),
        1,
        "the summary outlives the transcript it was made from"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM post_pass_jobs WHERE meeting_id = 'A'
               AND live_transcript_id IS NULL AND cleaned_transcript_id IS NULL"
        ),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM segments_fts WHERE segments_fts MATCH 'transcript'"),
        0,
        "the cascade must still retract the segment's words from search"
    );
}

fn repair(db: &Db, table: &str) -> Option<serde_json::Value> {
    db.conn()
        .query_row(
            "SELECT value FROM app_meta WHERE key = ?1",
            [format!("ownership_repair:{table}")],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .map(|v| serde_json::from_str(&v).unwrap())
}

fn names(conn: &Connection, kind: &str) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = ?1 AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    stmt.query_map([kind], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// A library that already holds mismatched rows — the state #336's archive
/// import left behind — migrates, and each table's repair is recorded.
#[test]
fn a_library_with_mismatched_rows_migrates_and_reports_the_repair() {
    let (_dir, path) = tmp_db();
    let (v11_triggers, v11_indexes);
    {
        let conn = raw(&path);
        apply_v11(&conn);
        v11_triggers = names(&conn, "trigger");
        v11_indexes = names(&conn, "index");

        meeting(&conn, "A", "Alpha");
        meeting(&conn, "B", "Bravo");
        meeting(&conn, "C", "Charlie");
        transcript(&conn, "tA", "A");
        transcript(&conn, "tB", "B");
        // C has no transcript at all.

        segment(&conn, "sA0", "tA", "A", 0, "alpha budget review").unwrap();
        segment(&conn, "sB0", "tB", "B", 0, "bravo hiring plan").unwrap();
        // Claims B, which has transcripts of its own: either id could be the
        // wrong one, so the row cannot be placed.
        segment(&conn, "sX", "tA", "B", 1, "zebra leakage phrase").unwrap();
        // Claims C, which has no transcript: the transcript is the only home
        // the row can have.
        segment(&conn, "sY", "tA", "C", 2, "yonder rehome phrase").unwrap();

        epoch(&conn, "eA", "A", "tA").unwrap();
        epoch(&conn, "eX", "B", "tA").unwrap();
        receipt(&conn, "rA", "A", "tA", "eA").unwrap();
        receipt(&conn, "rX", "B", "tA", "eX").unwrap();

        summary(&conn, "sumA", "A", Some("tA")).unwrap();
        summary(&conn, "sumX", "B", Some("tA")).unwrap();

        post_pass_job(&conn, "B", Some("tA"), Some("tB")).unwrap();
    }

    let db = Db::open(&path, &test_key()).unwrap();
    assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    let conn = db.conn();

    // Nothing the triggers and indexes did before is lost in the rebuild.
    let now_triggers = names(conn, "trigger");
    let now_indexes = names(conn, "index");
    assert!(
        v11_triggers.is_subset(&now_triggers),
        "triggers lost: {:?}",
        v11_triggers.difference(&now_triggers).collect::<Vec<_>>()
    );
    assert!(
        v11_indexes.is_subset(&now_indexes),
        "indexes lost: {:?}",
        v11_indexes.difference(&now_indexes).collect::<Vec<_>>()
    );

    let one = |sql: &str| -> Option<String> { conn.query_row(sql, [], |r| r.get(0)).ok() };
    assert_eq!(
        one("SELECT meeting_id FROM segments WHERE id = 'sY'").as_deref(),
        Some("A")
    );
    assert_eq!(one("SELECT meeting_id FROM segments WHERE id = 'sX'"), None);
    assert_eq!(
        one("SELECT meeting_id FROM segments WHERE id = 'sA0'").as_deref(),
        Some("A")
    );
    assert_eq!(
        one("SELECT meeting_id FROM segments WHERE id = 'sB0'").as_deref(),
        Some("B")
    );
    assert_eq!(one("SELECT id FROM speech_epochs WHERE id = 'eX'"), None);
    assert_eq!(one("SELECT id FROM speech_receipts WHERE id = 'rX'"), None);
    assert_eq!(
        one("SELECT id FROM speech_receipts WHERE id = 'rA'").as_deref(),
        Some("rA")
    );
    assert_eq!(
        one("SELECT coalesce(transcript_id, 'null') FROM summaries WHERE id = 'sumX'").as_deref(),
        Some("null"),
        "the summary stays with its meeting and loses only the pointer"
    );
    assert_eq!(
        one("SELECT transcript_id FROM summaries WHERE id = 'sumA'").as_deref(),
        Some("tA")
    );
    assert_eq!(
        one("SELECT coalesce(live_transcript_id, 'null') || '/' || cleaned_transcript_id FROM post_pass_jobs WHERE meeting_id = 'B'").as_deref(),
        Some("null/tB")
    );

    let report = |t: &str| repair(&db, t).unwrap_or_else(|| panic!("no repair report for {t}"));
    assert_eq!(
        report("segments"),
        serde_json::json!({"found": 2, "rehomed": 1, "removed": 1, "cleared": 0})
    );
    assert_eq!(
        report("speech_epochs"),
        serde_json::json!({"found": 1, "rehomed": 0, "removed": 1, "cleared": 0})
    );
    assert_eq!(
        report("speech_receipts"),
        serde_json::json!({"found": 1, "rehomed": 0, "removed": 1, "cleared": 0})
    );
    assert_eq!(
        report("summaries"),
        serde_json::json!({"found": 1, "rehomed": 0, "removed": 0, "cleared": 1})
    );
    assert_eq!(
        report("post_pass_jobs"),
        serde_json::json!({"found": 1, "rehomed": 0, "removed": 0, "cleared": 1})
    );

    let api: Vec<(String, u64)> = db
        .ownership_repairs()
        .unwrap()
        .into_iter()
        .map(|r| (r.table, r.found))
        .collect();
    assert_eq!(
        api,
        [
            ("post_pass_jobs", 1),
            ("segments", 2),
            ("speech_epochs", 1),
            ("speech_receipts", 1),
            ("summaries", 1),
        ]
        .map(|(t, n)| (t.to_owned(), n))
    );

    // Search still lines up with the rebuilt table: kept rows are found under
    // their (possibly new) meeting, and a removed row's words are gone.
    let hits = |q: &str| db.search(&SearchQuery::new(q)).unwrap();
    assert_eq!(hits("budget").len(), 1);
    let rehomed = hits("yonder");
    assert_eq!(rehomed.len(), 1);
    assert_eq!(rehomed[0].meeting_id, "A");
    assert!(
        hits("zebra").is_empty(),
        "a removed segment is still searchable"
    );

    // The rebuilt tables carry the rule from now on, and their triggers fire.
    assert_refused(
        segment(conn, "s-new-bad", "tA", "B", 9, "leak"),
        "segments after upgrade",
    );
    segment(conn, "s-new", "tB", "B", 9, "quokka arrives late").unwrap();
    assert_eq!(hits("quokka").len(), 1, "the FTS insert trigger was lost");
}

#[test]
fn a_clean_library_migrates_without_a_repair_report() {
    let (_dir, path) = tmp_db();
    {
        let conn = raw(&path);
        apply_v11(&conn);
        meeting(&conn, "A", "Alpha");
        transcript(&conn, "tA", "A");
        segment(&conn, "sA0", "tA", "A", 0, "alpha budget review").unwrap();
        summary(&conn, "sumA", "A", Some("tA")).unwrap();
    }
    let db = Db::open(&path, &test_key()).unwrap();
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM app_meta WHERE key GLOB 'ownership_repair:*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    let fk: bool = db
        .conn()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert!(fk, "foreign keys must be back on after the migration");
}

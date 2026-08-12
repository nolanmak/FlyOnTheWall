//! The deliverable of issue #37: **export, delete, re-import, and prove every
//! field came back**.
//!
//! # Why this file compares database snapshots rather than documents
//!
//! A round-trip test is the only way to know an export is lossless, and a
//! *loosely asserted* round-trip test is how a lossy one ships. The obvious
//! shapes of that mistake:
//!
//!   * compare the rendered Markdown — passes while every timestamp, every
//!     confidence and every note anchor is silently gone;
//!   * compare the exported document to a re-exported document — passes while
//!     export and import agree on the same wrong subset of columns;
//!   * compare field by field, by hand — passes for exactly the fields whoever
//!     wrote the test remembered, which is the same list whoever wrote the
//!     exporter remembered.
//!
//! So the comparison here is generated from the schema, not written out: for
//! every table SQLite reports, every column `PRAGMA table_info` reports, every
//! row, typed and canonicalized. A dropped field fails it. A dropped table
//! fails it. A column added in a future migration and forgotten by the exporter
//! fails it, without anyone editing this file.
//!
//! The values are **type-tagged** and floats compared **by their bits**. An
//! untagged comparison would let an integer arrive where a real belongs (STRICT
//! tables convert losslessly in both directions, so the value still reads
//! back), and a `==` on `f64` after a decimal round trip is the classic way to
//! pass while mangling the last significant digit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fotw_store::{
    ArchiveOptions, Db, DbKey, FTS_TABLES, LATEST_SCHEMA_VERSION, SearchQuery, SearchSource,
};
use rusqlite::types::Value;

mod common;
use common::test_key;

/// Every table migration 0001 creates. Repeated from `tests/schema.rs` on
/// purpose: if a migration adds a table, *both* lists must be updated, and this
/// one is the reminder that the new table also needs exporting.
const ALL_TABLES: &[&str] = &[
    "action_items",
    "app_meta",
    "devices",
    "folders",
    "meeting_participants",
    "meeting_tags",
    "meetings",
    "note_anchors",
    "notes",
    "people",
    "recordings",
    "segments",
    "settings",
    "summaries",
    "tags",
    "templates",
    "tombstones",
    "transcripts",
];

// --------------------------------------------------------------- the snapshot

fn is_fts_object(name: &str) -> bool {
    FTS_TABLES
        .iter()
        .any(|t| name == *t || name.starts_with(&format!("{t}_")))
}

/// One column value, encoded so that no two SQLite values of different type or
/// different bits can collide.
fn encode(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Integer(i) => format!("I:{i}"),
        // By bits, not by decimal text: `0.1 + 0.2` and `0.30000000000000004`
        // print identically at 15 digits and differ here, and `-0.0` is not
        // `0.0` even though `==` says it is.
        Value::Real(f) => format!("R:{:016x}", f.to_bits()),
        // escape_debug renders combining marks and zero-width joiners visibly,
        // so a failure caused by Unicode normalisation is legible in the diff
        // rather than "expected `café`, found `café`".
        Value::Text(s) => format!("T:{}", s.escape_debug()),
        Value::Blob(b) => {
            let mut out = String::from("B:");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out
        }
    }
}

fn table_names(db: &Db) -> Vec<String> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_schema
              WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
              ORDER BY name",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap)
        .filter(|n| !is_fts_object(n))
        .collect()
}

fn column_names(db: &Db, table: &str) -> Vec<String> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.map(Result::unwrap).collect()
}

/// `table -> sorted list of encoded rows`, over the whole library.
///
/// Rowids are never read: §9.7 invariant 1 reserves them for FTS joins and
/// forbids exporting them, and issue #37 says "every field reproduces except
/// rowids". Only named columns are visited, so they cannot leak in.
fn snapshot(db: &Db) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for table in table_names(db) {
        let cols = column_names(db, &table);
        let select = cols
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let conn = db.conn();
        let mut stmt = conn
            .prepare(&format!("SELECT {select} FROM \"{table}\""))
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let mut fields = Vec::with_capacity(cols.len());
                for (i, name) in cols.iter().enumerate() {
                    let v: Value = r.get(i)?;
                    fields.push(format!("{name}={}", encode(&v)));
                }
                Ok(fields.join("\u{1f}"))
            })
            .unwrap();
        let mut encoded: Vec<String> = rows.map(Result::unwrap).collect();
        encoded.sort();
        out.insert(table, encoded);
    }
    out
}

/// Assert two libraries hold exactly the same data, with a readable failure.
fn assert_same_library(a: &Db, b: &Db) {
    let sa = snapshot(a);
    let sb = snapshot(b);

    let ka: BTreeSet<&String> = sa.keys().collect();
    let kb: BTreeSet<&String> = sb.keys().collect();
    assert_eq!(ka, kb, "the two libraries do not have the same tables");

    let mut total_rows = 0usize;
    for (table, rows_a) in &sa {
        let rows_b = &sb[table];
        total_rows += rows_a.len();
        assert_eq!(
            rows_a.len(),
            rows_b.len(),
            "table `{table}`: {} rows before, {} after",
            rows_a.len(),
            rows_b.len()
        );
        for (i, (x, y)) in rows_a.iter().zip(rows_b).enumerate() {
            assert_eq!(
                x, y,
                "table `{table}` row {i} differs\n  source: {x}\n  import: {y}"
            );
        }
    }

    // A snapshot comparison of two empty libraries is trivially true, and a
    // comparison that skips a table is trivially true for that table. Neither
    // is allowed to pass quietly: every table must have carried rows.
    for (table, rows) in &sa {
        assert!(
            !rows.is_empty(),
            "table `{table}` was empty on both sides, so comparing it proved nothing"
        );
    }
    assert!(total_rows >= 40, "only {total_rows} rows compared");
}

// ------------------------------------------------------------------ the seed

/// A library holding, deliberately, every value shape that makes a naive
/// round-trip test pass while losing information.
///
/// The hazards, each with the failure it would otherwise hide:
///
///   * **NULL vs empty string** — `Option<String>` collapsed to `String`
///     turns one into the other and nothing else notices.
///   * **floating point** — a confidence written as decimal text and reparsed
///     loses the last bit; `-0.0`, `0.0`, subnormals and `0.1 + 0.2` are the
///     values that show it.
///   * **timestamps** — zero, negative (pre-1970) and far-future, because
///     "looks like a plausible date" validation eats all three.
///   * **empty collections** — an empty BLOB is not a missing BLOB, an empty
///     note is not a missing note, and a meeting with no transcript at all is
///     the one an exporter written against a happy path drops entirely.
///   * **Unicode** — the same text in NFC and NFD must stay two different
///     strings, and a ZWJ emoji sequence must not be re-segmented.
fn seed(db: &mut Db) {
    let sql = r#"
INSERT INTO devices (id, name, platform, app_version, is_self, last_seen_at_ms, created_at, updated_at)
VALUES ('dev-1', 'Laptop', 'macos', '0.1.0', 1, 1700000000000, 1700000000000, 1700000000000),
       ('dev-2', '', 'linux', '', 0, NULL, 0, 0);

-- Local provenance. Exported anyway: an archive that omits it is not a
-- complete backup, and "we did not think it mattered" is how fields vanish.
INSERT INTO app_meta (key, value, updated_at)
VALUES ('created_by', 'fotw 0.1.0', 1700000000000),
       ('empty_value', '', 0);

INSERT INTO settings (key, value, updated_at, lamport, origin_device_id)
VALUES ('theme', '"dark"', 1, 3, 'dev-1'),
       ('unicode', '"café / café"', 2, 0, 'dev-2');

-- Parent inserted AFTER the child on purpose: `folders.parent_id` is
-- self-referential, so an importer that inserts in file order without
-- deferring foreign keys fails right here.
INSERT INTO folders (id, name, parent_id, sort_key, created_at, updated_at, lamport, origin_device_id)
VALUES ('fold-child', 'Child', 'fold-root', 'b', 10, 11, 2, 'dev-1');
INSERT INTO folders (id, name, parent_id, sort_key, created_at, updated_at, lamport, origin_device_id)
VALUES ('fold-root', 'Root', NULL, '', 5, 6, 0, 'dev-1');

INSERT INTO templates (id, name, body_md, is_builtin, is_default, created_at, updated_at, lamport, origin_device_id)
VALUES ('tpl-1', 'Standup', '## Per person', 1, 1, 20, 21, 0, 'dev-1'),
       ('tpl-2', 'Empty body', '', 0, 0, 22, 23, 1, 'dev-2');

INSERT INTO people (id, display_name, email, is_self, voice_label, created_at, updated_at, lamport, origin_device_id)
VALUES ('per-1', 'Ana Ruiz', 'ana@example.com', 1, 'Speaker 0', 30, 31, 0, 'dev-1'),
       -- NULL email, NULL voice_label: distinct from '' and the pair the
       -- partial unique index treats as always-distinct.
       ('per-2', 'Unknown speaker', NULL, 0, NULL, 32, 33, 0, 'dev-1');

INSERT INTO tags (id, name, color, created_at, updated_at, lamport, origin_device_id)
VALUES ('tag-1', 'engineering', '#ff0000', 40, 41, 0, 'dev-1'),
       ('tag-2', 'ünïcode', NULL, 42, 43, 0, 'dev-1');

------------------------------------------------------------------ meeting one
INSERT INTO meetings (id, title, started_at_ms, ended_at_ms, duration_ms, tz,
                      folder_id, template_id, calendar_uid, calendar_source,
                      meeting_url, app_hint, state, language, disclosed,
                      retain_audio, retain_audio_days, created_at, updated_at,
                      lamport, origin_device_id)
VALUES ('mtg-1', 'Design review: café vs café', 1700000000000, 1700003600000, 3600000,
        'Europe/Berlin', 'fold-child', 'tpl-1', 'uid-abc', 'icloud',
        'https://meet.example.com/x', 'zoom.us', 'ready', 'en', 1,
        'days', 30, 1700000000000, 1700003600000, 4, 'dev-1');

INSERT INTO meeting_participants (id, meeting_id, person_id, display_name, email, role, speaker_label, created_at, updated_at, lamport, origin_device_id)
VALUES ('mp-1', 'mtg-1', 'per-1', 'Ana Ruiz', 'ana@example.com', 'organizer', 'Speaker 0', 50, 51, 0, 'dev-1'),
       -- Unmatched attendee: NULL person_id, NULL role, empty email.
       ('mp-2', 'mtg-1', NULL, 'Guest', '', NULL, NULL, 52, 53, 0, 'dev-1');

INSERT INTO meeting_tags (meeting_id, tag_id, created_at, lamport, origin_device_id)
VALUES ('mtg-1', 'tag-1', 60, 0, 'dev-1'), ('mtg-1', 'tag-2', 61, 1, 'dev-1');

INSERT INTO transcripts (id, meeting_id, provider, model, is_primary, language, audio_ms, cost_micros, raw_response_rel_path, created_at, updated_at, lamport, origin_device_id)
VALUES ('tr-1', 'mtg-1', 'deepgram', 'nova-3', 1, 'en', 3600000, 1234, 'media/2023/11/mtg-1/raw-deepgram.json.zst', 70, 71, 0, 'dev-1'),
       -- A second, non-primary transcript: re-transcribing must not destroy
       -- the old one (§9.3), so the export must not keep only the primary.
       ('tr-2', 'mtg-1', 'whisper', 'large-v3', 0, NULL, NULL, NULL, NULL, 72, 73, 1, 'dev-1');

INSERT INTO segments (id, transcript_id, meeting_id, idx, start_ms, end_ms, channel, speaker_label, person_id, text, confidence, is_final, words)
VALUES
  -- confidence NULL, speaker_label NULL, words NULL.
  ('seg-1', 'tr-1', 'mtg-1', 0, 0, 1500, 'mic', NULL, NULL, 'Morning.', NULL, 1, NULL),
  -- confidence exactly 1.0 -- an INTEGER 1 would read back the same through a
  -- STRICT REAL column, which is why the snapshot is type-tagged.
  ('seg-2', 'tr-1', 'mtg-1', 1, 1500, 3000, 'system', 'Speaker 0', 'per-1', 'Let''s start.', 1.0, 1, x'00ff10'),
  -- 0.1 + 0.2. Prints as 0.3 at 15 digits and is not 0.3.
  ('seg-3', 'tr-1', 'mtg-1', 2, 3000, 4500, 'mic', '', NULL, 'café', 0.30000000000000004, 1, x''),
  -- Negative zero, and an empty BLOB rather than a missing one.
  ('seg-4', 'tr-1', 'mtg-1', 3, 4500, 6000, 'mic', 'Speaker 1', NULL, 'café', -0.0, 0, x''),
  -- Subnormal, and the smallest positive double.
  ('seg-5', 'tr-1', 'mtg-1', 4, 6000, 7500, 'system', 'Speaker 1', NULL, '👩‍💻 shipped it', 5e-324, 1, x'6162630000ff'),
  -- Text carrying a newline, a lone CR, a tab and RTL script.
  ('seg-6', 'tr-1', 'mtg-1', 5, 7500, 9000, 'mic', NULL, NULL, 'line one' || char(10) || 'line' || char(13) || 'two' || char(9) || 'مرحبا', 0.0, 1, NULL);

INSERT INTO notes (id, meeting_id, body_md, created_at, updated_at, lamport, origin_device_id)
VALUES ('note-1', 'mtg-1', '- ship it' || char(10) || '- café', 80, 81, 7, 'dev-1');

INSERT INTO note_anchors (id, note_id, meeting_id, block_idx, block_text, typed_at_ms)
VALUES ('na-1', 'note-1', 'mtg-1', 0, '- ship it', 12000),
       -- typed_at_ms is ms from meeting start and may legitimately be 0.
       ('na-2', 'note-1', 'mtg-1', 1, '', 0);

INSERT INTO summaries (id, meeting_id, version, template_id, transcript_id, provider, model, prompt_hash, body_md, coverage, input_tokens, output_tokens, cost_micros, is_current, created_at, origin_device_id)
VALUES
  -- Every version is kept (§9.3): an export that writes only the current one
  -- silently destroys the history the append-only design exists to provide.
  ('sum-1', 'mtg-1', 1, 'tpl-1', 'tr-1', 'anthropic', 'claude-opus-5', 'aa00', '# v1', 0.7, 100, 200, 300, 0, 90, 'dev-1'),
  ('sum-2', 'mtg-1', 2, NULL, NULL, 'ollama', 'llama3', 'bb11', '# v2', NULL, NULL, NULL, NULL, 0, 91, 'dev-1'),
  ('sum-3', 'mtg-1', 3, 'tpl-2', 'tr-2', 'anthropic', 'claude-sonnet-5', 'cc22', '# v3 café', 0.9999999999999999, 0, 0, 0, 1, 92, 'dev-2');

INSERT INTO action_items (id, meeting_id, summary_id, kind, text, owner_person_id, owner_label, due_ms, due_raw, confidence, evidence_segment_ids, evidence_quote, status, created_at, updated_at, lamport, origin_device_id)
VALUES ('ai-1', 'mtg-1', 'sum-3', 'action_item', 'Ship the exporter', 'per-1', 'Speaker 0', 1700100000000, 'end of next sprint', 'explicit', '["seg-2","seg-3"]', 'Let''s start.', 'open', 100, 101, 0, 'dev-1'),
       -- The null-owner case §8.5 calls load-bearing, plus the documented
       -- empty evidence array.
       ('ai-2', 'mtg-1', NULL, 'decision', 'Use a directory, not a ZIP', NULL, NULL, NULL, NULL, 'implied', '[]', NULL, 'done', 102, 103, 1, 'dev-1');

INSERT INTO recordings (id, meeting_id, channel, rel_path, codec, container, sample_rate, channels, bitrate_bps, duration_ms, bytes, sha256, encrypted, state, purge_after_ms, created_at, updated_at, deleted_at, lamport, origin_device_id)
VALUES ('rec-1', 'mtg-1', 'mic', 'media/2023/11/mtg-1/mic.opus.age', 'opus', 'ogg', 48000, 1, 24000, 3600000, 10800000, 'deadbeef', 1, 'complete', 1702592000000, 110, 111, NULL, 0, 'dev-1'),
       -- Audio already purged: the row survives so the UI can say when.
       ('rec-2', 'mtg-1', 'system', 'media/2023/11/mtg-1/system.opus.age', 'opus', 'ogg', 48000, 1, 24000, NULL, NULL, NULL, 0, 'deleted', NULL, 112, 113, 1702592000000, 2, 'dev-1');

------------------------------------------------------------------ meeting two
-- Empty in every direction: no transcript, no note, no summary, no tag, no
-- participant, no recording, empty title, still recording. The meeting an
-- exporter written against the happy path silently omits.
INSERT INTO meetings (id, title, started_at_ms, ended_at_ms, duration_ms, tz,
                      folder_id, template_id, calendar_uid, calendar_source,
                      meeting_url, app_hint, state, language, disclosed,
                      retain_audio, retain_audio_days, created_at, updated_at,
                      lamport, origin_device_id)
VALUES ('mtg-2', '', 0, NULL, NULL, 'UTC', NULL, NULL, NULL, NULL, NULL, NULL,
        'recording', NULL, 0, 'default', NULL, 0, 0, 0, 'dev-2');

---------------------------------------------------------------- meeting three
-- Extreme timestamps: pre-1970 and far future.
INSERT INTO meetings (id, title, started_at_ms, ended_at_ms, duration_ms, tz,
                      folder_id, template_id, calendar_uid, calendar_source,
                      meeting_url, app_hint, state, language, disclosed,
                      retain_audio, retain_audio_days, created_at, updated_at,
                      lamport, origin_device_id)
VALUES ('mtg-3', 'Historic', -86400000, 4102444800000, 4102531200000, 'Pacific/Kiritimati',
        NULL, NULL, '', '', '', '', 'failed', '', 0, 'forever', NULL,
        -1, 4102444800000, 9223372036854775807, 'dev-2');

-- A transcript with zero segments: an exporter that keys off "has segments"
-- drops the row and the provenance with it.
INSERT INTO transcripts (id, meeting_id, provider, model, is_primary, language, audio_ms, cost_micros, raw_response_rel_path, created_at, updated_at, lamport, origin_device_id)
VALUES ('tr-3', 'mtg-3', 'deepgram', 'nova-3', 1, NULL, 0, 0, '', 120, 121, 0, 'dev-2');

-- A note with an empty body and zero anchors.
INSERT INTO notes (id, meeting_id, body_md, created_at, updated_at, lamport, origin_device_id)
VALUES ('note-3', 'mtg-3', '', 130, 131, 0, 'dev-2');

INSERT INTO tombstones (id, kind, deleted_at, origin_device_id, lamport)
VALUES ('gone-1', 'meeting', 1700000000000, 'dev-1', 3),
       ('gone-2', 'note', 0, 'dev-2', 0);
"#;
    // Deferred foreign keys, for the same reason the importer defers them: the
    // fixture inserts `fold-child` before `fold-root` on purpose, so that an
    // importer which does not defer fails on this data rather than passing on
    // a fixture arranged to be easy.
    let tx = db.conn_mut().transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", true).unwrap();
    tx.execute_batch(sql).unwrap();
    tx.commit().unwrap();
}

fn open_at(path: &Path) -> Db {
    Db::open(path, &test_key()).unwrap()
}

/// Source library, its temp dir, and the archive destination.
struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    archive: std::path::PathBuf,
    fresh: std::path::PathBuf,
}

fn fixture() -> (Fixture, Db) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("source");
    let archive = dir.path().join("archive");
    let fresh = dir.path().join("fresh");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&fresh).unwrap();
    let mut db = open_at(&root.join("db.sqlite3"));
    seed(&mut db);
    (
        Fixture {
            _dir: dir,
            root,
            archive,
            fresh,
        },
        db,
    )
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_fixture_covers_every_table_in_the_schema() {
    // Guards the guard. A snapshot comparison over tables the fixture never
    // populates is a comparison of two empty sets, and every table below is
    // one an exporter could forget.
    let (_f, db) = fixture();
    let snap = snapshot(&db);

    let tables: Vec<&str> = snap.keys().map(String::as_str).collect();
    assert_eq!(tables, ALL_TABLES, "the schema gained or lost a table");

    for table in ALL_TABLES {
        assert!(
            !snap[*table].is_empty(),
            "table `{table}` is empty in the fixture, so the round trip proves \
             nothing about it"
        );
    }
}

#[test]
fn a_library_survives_export_and_import_into_a_fresh_one() {
    let (f, source) = fixture();

    let report = source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();
    assert_eq!(report.meetings, 3);

    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    let imported = dest.import_library(&f.archive).unwrap();
    assert_eq!(imported.meetings, 3);
    assert!(imported.conflicts.is_empty(), "{:?}", imported.conflicts);

    assert_same_library(&source, &dest);
}

#[test]
fn importing_the_same_archive_twice_changes_nothing() {
    // "Idempotent on conflict" (issue #37). The second run must not duplicate
    // rows, must not fail, and must not report a conflict for a row that is
    // already exactly what the archive says.
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    let first = dest.import_library(&f.archive).unwrap();
    let after_first = snapshot(&dest);

    let second = dest.import_library(&f.archive).unwrap();
    assert_eq!(snapshot(&dest), after_first, "a second import changed data");
    assert!(second.conflicts.is_empty(), "{:?}", second.conflicts);
    assert_eq!(second.rows_inserted, 0);
    assert_eq!(second.rows_already_present, first.rows_inserted);
}

#[test]
fn export_a_meeting_delete_it_and_re_import_reproduces_every_field() {
    // Issue #37's stated "done when", to the letter.
    let (f, mut db) = fixture();
    let before = snapshot(&db);

    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let outcome = db.delete_meeting("mtg-1").unwrap();
    assert!(outcome.existed);
    assert_ne!(snapshot(&db), before, "delete_meeting did nothing");

    let report = db.import_library(&f.archive).unwrap();
    assert!(report.rows_inserted > 0);

    // Everything is back except the tombstone the delete left behind, which is
    // a *new* fact about this library and must not be undone by an import.
    let after = snapshot(&db);
    for (table, rows) in &before {
        if table == "tombstones" {
            continue;
        }
        assert_eq!(rows, &after[table], "table `{table}` did not come back");
    }
    assert_eq!(
        after["tombstones"].len(),
        before["tombstones"].len() + 1,
        "the delete's tombstone was lost or duplicated"
    );
}

#[test]
fn the_importer_reuses_the_original_uuids() {
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();
    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    dest.import_library(&f.archive).unwrap();

    // Not "three meetings exist" -- these exact ids, because a re-import that
    // minted fresh ids would still satisfy a count and would break every
    // cross-reference a user has to these meetings.
    for id in ["mtg-1", "mtg-2", "mtg-3"] {
        assert_eq!(dest.meetings().get(id).unwrap().id, id);
    }
    assert_eq!(
        dest.meetings()
            .current_summary("mtg-1")
            .unwrap()
            .unwrap()
            .id,
        "sum-3"
    );
}

#[test]
fn derived_search_state_is_rebuilt_by_the_import_not_carried_by_it() {
    // §9.7 invariant 6: derived state is rebuildable and never synced. The
    // archive holds no FTS rows at all, so search working afterwards proves
    // the triggers fired on insert.
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let text = std::fs::read_to_string(f.archive.join("library.json")).unwrap();
    assert!(!text.contains("_fts"), "the archive carried derived state");

    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    dest.import_library(&f.archive).unwrap();
    dest.verify_search_index().unwrap();

    // Needles from three different indexes, so a partly-wired trigger set
    // cannot pass this: a transcript segment, the user's note, a title.
    for (needle, source) in [
        ("shipped", SearchSource::Transcript),
        ("ship", SearchSource::Note),
        ("Historic", SearchSource::Title),
    ] {
        let hits = dest.search(&SearchQuery::new(needle)).unwrap();
        assert!(
            hits.iter().any(|h| h.source == source),
            "`{needle}` is not searchable in the {source:?} index after import"
        );
    }
}

#[test]
fn an_archive_from_a_newer_schema_is_refused_rather_than_half_imported() {
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let path = f.archive.join("library.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let bumped = text.replace(
        &format!("\"schema_version\": {LATEST_SCHEMA_VERSION}"),
        &format!("\"schema_version\": {}", LATEST_SCHEMA_VERSION + 1),
    );
    assert_ne!(bumped, text, "schema_version is not in the manifest");
    std::fs::write(&path, bumped).unwrap();

    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    let err = dest.import_library(&f.archive).unwrap_err();
    assert!(err.to_string().contains("newer"), "unhelpful error: {err}");
    // Nothing landed.
    assert!(dest.meetings().list(10, 0).unwrap().is_empty());
}

#[test]
fn a_corrupt_meeting_file_fails_the_import_without_writing_half_of_it() {
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();
    std::fs::write(f.archive.join("meetings/mtg-2.json"), "{ not json").unwrap();

    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    assert!(dest.import_library(&f.archive).is_err());

    // One transaction for the whole archive: a partial library is worse than
    // none, because the user cannot tell which half is missing.
    assert!(
        dest.meetings().list(10, 0).unwrap().is_empty(),
        "a failed import left rows behind"
    );
}

// ------------------------------------------------------ per-meeting documents

#[test]
fn a_single_meeting_document_round_trips_through_json() {
    let (_f, db) = fixture();
    let doc = db.export_meeting("mtg-1").unwrap();
    let json = doc.to_json_pretty();
    let back = fotw_store::MeetingDoc::from_json(&json).unwrap();
    assert_eq!(doc, back, "meeting@1 is not a faithful serialization");
}

#[test]
fn the_meeting_document_carries_every_summary_version_and_every_anchor() {
    let (_f, db) = fixture();
    let doc = db.export_meeting("mtg-1").unwrap();
    assert_eq!(doc.schema, fotw_store::MEETING_SCHEMA);
    assert_eq!(doc.summaries.len(), 3, "summary history was truncated");
    assert_eq!(doc.note_anchors.len(), 2, "note anchors were dropped");
    assert_eq!(doc.segments.len(), 6);
    assert_eq!(
        doc.transcripts.len(),
        2,
        "the non-primary transcript was dropped"
    );
    assert_eq!(doc.action_items.len(), 2);
    assert_eq!(doc.recordings.len(), 2);
    assert_eq!(doc.participants.len(), 2);
    assert_eq!(doc.meeting_tags.len(), 2);
}

#[test]
fn word_timings_survive_as_bytes_including_the_empty_blob() {
    let (_f, db) = fixture();
    let doc = db.export_meeting("mtg-1").unwrap();
    let by_idx: BTreeMap<i64, &fotw_store::export::SegmentRow> =
        doc.segments.iter().map(|s| (s.idx, s)).collect();

    assert!(
        by_idx[&0].words.is_none(),
        "NULL words became something else"
    );
    assert_eq!(by_idx[&1].words.as_ref().unwrap().0, vec![0x00, 0xff, 0x10]);
    // An empty BLOB is not a missing one, and base64 of nothing is the empty
    // string -- the exact place `Some(vec![])` degrades to `None`.
    assert_eq!(by_idx[&2].words.as_ref().unwrap().0, Vec::<u8>::new());
    assert_eq!(by_idx[&4].words.as_ref().unwrap().0, b"abc\0\0\xff");
}

#[test]
fn the_markdown_export_is_a_valid_obsidian_note() {
    // EXP-01: the YAML frontmatter is what makes the same file a valid
    // Obsidian note, so it is the part that must be exactly right.
    let (_f, db) = fixture();
    let md = db.export_meeting("mtg-1").unwrap().to_markdown();

    assert!(md.starts_with("---\n"), "no frontmatter fence");
    let end = md[4..].find("\n---\n").expect("frontmatter is not closed") + 4;
    let front = &md[4..end];

    for key in [
        "id:",
        "title:",
        "date:",
        "duration:",
        "attendees:",
        "tags:",
        "folder:",
    ] {
        assert!(front.contains(key), "frontmatter lacks {key}\n{front}");
    }
    assert!(front.contains("mtg-1"));
    assert!(front.contains("engineering"));
    assert!(front.contains("Ana Ruiz"));

    let body = &md[end..];
    assert!(body.contains("# v3"), "the current summary is missing");
    assert!(body.contains("ship it"), "the user's notes are missing");
    assert!(
        body.contains("Ship the exporter"),
        "action items are missing"
    );
}

#[test]
fn a_title_that_would_break_the_frontmatter_is_quoted() {
    // A colon in a title is ordinary and turns unquoted YAML into either a
    // parse error or a different document. Obsidian silently shows the raw
    // text when that happens, which reads as "the export is broken".
    let (_f, db) = fixture();
    db.conn()
        .execute(
            "UPDATE meetings SET title = ?1 WHERE id = 'mtg-1'",
            ["Q3: \"planning\" — 100% #done\nand a newline"],
        )
        .unwrap();
    let md = db.export_meeting("mtg-1").unwrap().to_markdown();
    let front = &md[4..md[4..].find("\n---\n").unwrap() + 4];
    assert!(front.contains("title: "), "{front}");
    // One logical line, whatever the title contained.
    let title_line = front
        .lines()
        .find(|l| l.starts_with("title: "))
        .expect("title line");
    assert!(title_line.contains("Q3"), "{title_line}");
    assert!(!title_line.contains('\n'));
    // And it parses back out to exactly the original.
    let parsed = fotw_store::export::parse_yaml_scalar(&title_line["title: ".len()..]);
    assert_eq!(parsed, "Q3: \"planning\" — 100% #done\nand a newline");
}

#[test]
fn the_plain_text_export_timestamps_every_line() {
    let (_f, db) = fixture();
    let txt = db.export_meeting("mtg-1").unwrap().to_plain_text();
    // `[00:12:34] Alice: …` (issue #37).
    assert!(txt.contains("[00:00:01] Speaker 0: Let's start."), "{txt}");
    assert!(txt.contains("[00:00:00]"), "{txt}");
    // A segment with no diarisation still gets a line rather than vanishing.
    assert!(txt.contains("Morning."), "{txt}");
}

#[test]
fn the_clipboard_carries_both_a_text_and_an_html_flavor() {
    // EXP-02: a paste lands rich in Slack/Notion and plain in an editor.
    let (_f, db) = fixture();
    let doc = db.export_meeting("mtg-1").unwrap();
    let clip = doc.to_clipboard();
    assert!(!clip.text.is_empty());
    assert!(clip.html.contains("<h1"), "{}", clip.html);
    // Whatever a summary body contains, it must not become live markup.
    assert!(!clip.html.contains("<script"), "{}", clip.html);
}

#[test]
fn html_escapes_content_that_would_otherwise_become_markup() {
    let (_f, db) = fixture();
    db.conn()
        .execute(
            "UPDATE summaries SET body_md = ?1 WHERE id = 'sum-3'",
            ["<script>alert(1)</script> & <b>bold</b>"],
        )
        .unwrap();
    let clip = db.export_meeting("mtg-1").unwrap().to_clipboard();
    assert!(clip.html.contains("&lt;script&gt;"), "{}", clip.html);
    assert!(!clip.html.contains("<script>"), "{}", clip.html);
    assert!(clip.html.contains("&amp;"), "{}", clip.html);
}

// ------------------------------------------------------------ archive shape

#[test]
fn the_archive_is_one_file_per_meeting_plus_a_manifest() {
    let (f, db) = fixture();
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    assert!(f.archive.join("library.json").is_file());
    assert!(f.archive.join("README.txt").is_file());
    for id in ["mtg-1", "mtg-2", "mtg-3"] {
        assert!(
            f.archive.join(format!("meetings/{id}.json")).is_file(),
            "{id} has no file"
        );
        assert!(f.archive.join(format!("markdown/{id}.md")).is_file());
    }
}

#[test]
fn the_archive_says_in_writing_that_it_is_unencrypted() {
    // The library is SQLCipher-encrypted; this directory is not. A user who
    // does not understand that has just written every meeting they ever
    // recorded to disk in the clear.
    let (f, db) = fixture();
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let readme = std::fs::read_to_string(f.archive.join("README.txt")).unwrap();
    let lowered = readme.to_lowercase();
    assert!(lowered.contains("not encrypted"), "{readme}");
    assert!(
        lowered.contains("plain text") || lowered.contains("plaintext"),
        "{readme}"
    );

    let manifest = std::fs::read_to_string(f.archive.join("library.json")).unwrap();
    assert!(manifest.contains("\"encryption\": \"none\""), "{manifest}");
}

#[test]
fn progress_is_reported_once_per_meeting() {
    let (f, db) = fixture();
    let mut seen = Vec::new();
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |p| {
        seen.push((p.done, p.total, p.meeting_id.clone()));
    })
    .unwrap();

    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].0, 1);
    assert_eq!(seen[2].0, 3);
    assert!(seen.iter().all(|(_, total, _)| *total == 3));
    let ids: BTreeSet<String> = seen.into_iter().map(|(_, _, id)| id).collect();
    assert_eq!(ids.len(), 3);
}

#[test]
fn a_resumed_export_only_rewrites_what_is_missing_or_stale() {
    let (f, db) = fixture();
    let opts = ArchiveOptions {
        resume: true,
        ..ArchiveOptions::default()
    };
    db.export_library(&f.archive, &opts, &mut |_| {}).unwrap();

    // Simulate an interrupted run: one file gone, one truncated.
    std::fs::remove_file(f.archive.join("meetings/mtg-2.json")).unwrap();
    std::fs::write(f.archive.join("meetings/mtg-3.json"), "{ truncated").unwrap();

    let report = db.export_library(&f.archive, &opts, &mut |_| {}).unwrap();
    assert_eq!(report.meetings, 3);
    assert_eq!(report.skipped, 1, "the intact file was rewritten");
    assert_eq!(
        report.written, 2,
        "a missing or corrupt file was not repaired"
    );

    // And the result is still a complete, importable archive.
    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    dest.import_library(&f.archive).unwrap();
    assert_same_library(&db, &dest);
}

#[test]
fn audio_is_opt_in_and_its_size_is_knowable_first() {
    let (f, db) = fixture();
    // The media file the `complete` recording points at.
    let media = f.root.join("media/2023/11/mtg-1/mic.opus.age");
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    std::fs::write(&media, vec![7u8; 4096]).unwrap();

    let projection = db.projected_archive_size(&f.root).unwrap();
    assert_eq!(projection.meetings, 3);
    // Reads `recordings.bytes` for rows that still have audio, and must not
    // count the one whose bytes were already purged.
    assert_eq!(projection.audio_bytes, 10_800_000);
    assert!(projection.audio_files == 1, "{projection:?}");

    // Default: no audio in the archive.
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();
    assert!(
        !f.archive.join("media").exists(),
        "audio was exported by default"
    );

    let with_audio = ArchiveOptions {
        include_audio: true,
        data_root: Some(f.root.clone()),
        ..ArchiveOptions::default()
    };
    let report = db
        .export_library(&f.archive, &with_audio, &mut |_| {})
        .unwrap();
    assert_eq!(report.audio_files, 1);
    let copied = f.archive.join("media/2023/11/mtg-1/mic.opus.age");
    assert_eq!(std::fs::read(&copied).unwrap(), vec![7u8; 4096]);
}

#[test]
fn the_template_files_travel_with_the_archive() {
    // Templates are files outside the database (issue #36), so an archive that
    // omits them is not a backup of everything the user owns.
    let (f, db) = fixture();
    let templates = f.root.join("templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(templates.join("mine.md"), "---\nname: Mine\n---\nmy body\n").unwrap();

    let opts = ArchiveOptions {
        templates_dir: Some(templates.clone()),
        ..ArchiveOptions::default()
    };
    db.export_library(&f.archive, &opts, &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(f.archive.join("templates/mine.md")).unwrap(),
        "---\nname: Mine\n---\nmy body\n"
    );

    let restore = f.fresh.join("templates");
    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    dest.import_library_with(&f.archive, Some(&restore), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(restore.join("mine.md")).unwrap(),
        "---\nname: Mine\n---\nmy body\n"
    );
}

#[test]
fn a_conflicting_row_is_reported_rather_than_silently_skipped() {
    // Importing into a library that already holds a *different* row with the
    // same natural key must not quietly drop the archive's version. The
    // partial unique indexes (`templates_default_uidx` and friends) make
    // `INSERT OR IGNORE` a silent-loss machine, which is why the importer does
    // not use it.
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let mut dest = open_at(&f.fresh.join("db.sqlite3"));
    dest.conn()
        .execute(
            "INSERT INTO templates (id, name, body_md, is_builtin, is_default, created_at, updated_at, lamport, origin_device_id)
             VALUES ('other', 'Someone else''s default', '', 0, 1, 1, 1, 0, 'dev-9')",
            [],
        )
        .unwrap();

    let report = dest.import_library(&f.archive).unwrap();
    assert!(
        report
            .conflicts
            .iter()
            .any(|c| c.table == "templates" && c.id == "tpl-1"),
        "the clashing default template was skipped silently: {:?}",
        report.conflicts
    );
}

#[test]
fn export_refuses_a_destination_that_is_already_something_else() {
    let (f, db) = fixture();
    std::fs::create_dir_all(&f.archive).unwrap();
    std::fs::write(f.archive.join("holiday.jpg"), b"not an archive").unwrap();
    let err = db
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap_err();
    assert!(err.to_string().contains("not empty"), "{err}");
}

#[test]
fn a_key_is_never_written_into_an_archive() {
    // KEY-01 by way of §10: the archive is plaintext, so anything secret that
    // reached it is unrecoverable. Nothing in the schema holds key material
    // today; this is the assertion that notices when something starts to.
    let (f, db) = fixture();
    db.conn()
        .execute(
            "INSERT INTO settings (key, value, updated_at, lamport, origin_device_id)
             VALUES ('never', '\"zqxjkvbwphgf-not-a-real-key\"', 0, 0, 'dev-1')",
            [],
        )
        .unwrap();
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    // The needle IS present, because settings are user data and do round-trip.
    // The point of the test is the shape: a future secret column must be
    // excluded here deliberately, and this is where that decision surfaces.
    let manifest = std::fs::read_to_string(f.archive.join("library.json")).unwrap();
    assert!(manifest.contains("zqxjkvbwphgf-not-a-real-key"));

    let key_hex = "0101010101010101010101010101010101010101010101010101010101010101";
    for entry in walk(&f.archive) {
        let bytes = std::fs::read(&entry).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(key_hex),
            "{} contains the database key",
            entry.display()
        );
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn the_archive_is_readable_without_the_database_key() {
    // The whole point of "no lock-in": the archive must be usable by something
    // that is not FlyOnTheWall. Parsing it with a plain JSON reader and no key
    // is the proof.
    let (f, db) = fixture();
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();
    let raw = std::fs::read_to_string(f.archive.join("meetings/mtg-1.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["schema"], "flyonthewall/meeting@1");
    assert_eq!(v["meeting"]["id"], "mtg-1");
    assert_eq!(v["summaries"].as_array().unwrap().len(), 3);
    // Nulls are nulls, not the string "null" and not missing keys.
    assert!(v["segments"][0]["confidence"].is_null());
    assert!(
        v["segments"][0]
            .as_object()
            .unwrap()
            .contains_key("confidence")
    );
}

/// Every column of every table must appear in the exported JSON, and this is
/// checked against `PRAGMA table_info` rather than against a list somebody
/// typed. A migration that adds a column and forgets the exporter fails here.
#[test]
fn every_column_of_every_table_appears_in_the_archive() {
    let (f, db) = fixture();
    db.export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(f.archive.join("library.json")).unwrap())
            .unwrap();
    let meeting: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(f.archive.join("meetings/mtg-1.json")).unwrap(),
    )
    .unwrap();

    // Which JSON array each table's rows live in.
    let places: &[(&str, &serde_json::Value, &str)] = &[
        ("devices", &manifest, "devices"),
        ("app_meta", &manifest, "app_meta"),
        ("settings", &manifest, "settings"),
        ("folders", &manifest, "folders"),
        ("templates", &manifest, "templates"),
        ("people", &manifest, "people"),
        ("tags", &manifest, "tags"),
        ("tombstones", &manifest, "tombstones"),
        ("meetings", &meeting, "meeting"),
        ("meeting_participants", &meeting, "participants"),
        ("meeting_tags", &meeting, "meeting_tags"),
        ("transcripts", &meeting, "transcripts"),
        ("segments", &meeting, "segments"),
        ("notes", &meeting, "notes"),
        ("note_anchors", &meeting, "note_anchors"),
        ("summaries", &meeting, "summaries"),
        ("action_items", &meeting, "action_items"),
        ("recordings", &meeting, "recordings"),
    ];
    let covered: BTreeSet<&str> = places.iter().map(|(t, _, _)| *t).collect();
    let expected: BTreeSet<&str> = ALL_TABLES.iter().copied().collect();
    assert_eq!(covered, expected, "a table has no home in the archive");

    for (table, doc, key) in places {
        let node = &doc[key];
        let obj = if node.is_array() {
            node.as_array()
                .unwrap()
                .first()
                .unwrap_or_else(|| panic!("no rows exported for `{table}`"))
        } else {
            node
        };
        let keys: BTreeSet<&str> = obj
            .as_object()
            .unwrap_or_else(|| panic!("`{key}` is not an object"))
            .keys()
            .map(String::as_str)
            .collect();
        for column in column_names(&db, table) {
            assert!(
                keys.contains(column.as_str()),
                "column `{table}.{column}` is not in the archive; \
                 the exporter and the schema have drifted"
            );
        }
    }
}

#[test]
fn a_meeting_that_does_not_exist_is_an_error_not_an_empty_document() {
    let (_f, db) = fixture();
    assert!(db.export_meeting("no-such-meeting").is_err());
}

#[test]
fn an_empty_library_exports_and_imports_without_special_casing() {
    let dir = tempfile::TempDir::new().unwrap();
    let src = Db::open(dir.path().join("a/db.sqlite3"), &test_key()).unwrap();
    let archive = dir.path().join("archive");
    let report = src
        .export_library(&archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();
    assert_eq!(report.meetings, 0);

    let mut dest = Db::open(dir.path().join("b/db.sqlite3"), &test_key()).unwrap();
    dest.import_library(&archive).unwrap();
    assert!(dest.meetings().list(10, 0).unwrap().is_empty());
}

#[test]
fn the_import_works_against_a_library_opened_with_a_different_key() {
    // Nothing about an archive is tied to the key that produced it -- that is
    // what "the export is the portability guarantee" means.
    let (f, source) = fixture();
    source
        .export_library(&f.archive, &ArchiveOptions::default(), &mut |_| {})
        .unwrap();

    let other = DbKey::from_bytes([0x5a; 32]);
    let mut dest = Db::open(f.fresh.join("db.sqlite3"), &other).unwrap();
    dest.import_library(&f.archive).unwrap();
    assert_same_library(&source, &dest);
}

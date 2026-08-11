//! Schema lint: the rules from docs/REQUIREMENTS.md 9.3 and 9.7 that a review
//! comment cannot enforce.
//!
//! §9.7 says these invariants are "enforced by CI lint over migration SQL".
//! This file is that lint. It reads the *applied* schema rather than the
//! `.sql` text, so it also catches a migration whose DDL parses but does not
//! do what it looks like it does.

use std::collections::BTreeSet;

use fotw_store::{Db, FTS_TABLES};
use rusqlite::{Connection, OptionalExtension};

mod common;
use common::test_key;

/// True for the FTS5 virtual tables from migration 0002 and for the shadow
/// tables SQLite creates underneath them (`*_data`, `*_idx`, `*_docsize`,
/// `*_config`).
///
/// The house rules below are rules about *our* schema. An FTS5 index is not a
/// table we wrote — its shape is chosen by SQLite, it cannot be `STRICT`, and
/// its shadow tables use implicit integer keys that §9.7 invariant 1
/// specifically permits ("implicit rowids exist only as FTS join keys"). Rather
/// than weaken each rule with an exception, they are excluded here once, and
/// `the_fts_indexes_are_external_content_over_the_source_tables` below is what
/// bounds the exclusion so it cannot become a hiding place.
fn is_fts_object(name: &str) -> bool {
    FTS_TABLES
        .iter()
        .any(|t| name == *t || name.starts_with(&format!("{t}_")))
}

/// Every table migration 0001 is required to create.
const EXPECTED_TABLES: &[&str] = &[
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

fn open() -> Db {
    Db::open_in_memory(&test_key()).unwrap()
}

/// Every table in the applied schema, FTS objects included.
fn all_table_names(conn: &Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

/// The tables this project wrote by hand — the ones the house rules govern.
fn table_names(conn: &Connection) -> BTreeSet<String> {
    all_table_names(conn)
        .into_iter()
        .filter(|n| !is_fts_object(n))
        .collect()
}

/// `(name, type, notnull, pk)` for every column of `table`.
fn columns(conn: &Connection, table: &str) -> Vec<(String, String, bool, i64)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, i64>(5)?,
            ))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn column_names(conn: &Connection, table: &str) -> BTreeSet<String> {
    columns(conn, table).into_iter().map(|c| c.0).collect()
}

fn ddl(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT name, sql FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    rows.map(Result::unwrap)
        .filter(|(name, _): &(String, String)| !is_fts_object(name))
        .collect()
}

/// The `CREATE VIRTUAL TABLE` text of one FTS index.
fn fts_ddl(conn: &Connection, table: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

#[test]
fn migration_0001_creates_every_table_the_spec_names() {
    let db = open();
    let found = table_names(db.conn());
    let expected: BTreeSet<String> = EXPECTED_TABLES.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        found, expected,
        "the applied schema does not match §9.3's table list"
    );
}

/// §9.4 names the four things a person searches for. One index each.
#[test]
fn migration_0002_creates_the_four_fts_tables() {
    let db = open();
    let all = all_table_names(db.conn());
    for t in FTS_TABLES {
        assert!(all.contains(t), "missing FTS index `{t}`: {all:?}");
    }
    assert_eq!(
        FTS_TABLES.len(),
        4,
        "§9.4 asks for exactly four: segments, notes, summaries, meetings"
    );
}

/// The bound on the STRICT/AUTOINCREMENT exemption above.
///
/// `content='<table>'` is what makes these indexes cheap: the tokens live in
/// the index, the columns are read back through the shared rowid, and the text
/// is stored once. A *contentless-with-content* or default FTS5 table would
/// instead keep a second full copy of every transcript in a `%_content` shadow
/// table — doubling §9.5's 250 MB/year of text — and would make the delete
/// triggers unnecessary, which is precisely the property this schema must not
/// quietly acquire.
#[test]
fn the_fts_indexes_are_external_content_over_the_source_tables() {
    let db = open();
    let all = all_table_names(db.conn());
    for (fts, source) in [
        ("meetings_fts", "meetings"),
        ("notes_fts", "notes"),
        ("summaries_fts", "summaries"),
        ("segments_fts", "segments"),
    ] {
        let sql = fts_ddl(db.conn(), fts);
        assert!(
            sql.contains(&format!("content = '{source}'")),
            "`{fts}` is not external content over `{source}`: {sql}"
        );
        assert!(
            !all.contains(&format!("{fts}_content")),
            "`{fts}` kept a private copy of the text in {fts}_content"
        );
    }
}

/// §9.4: `unicode61 remove_diacritics 2` everywhere, and NOT `porter`.
///
/// The behavioural half of this — that a product name does not match an
/// unrelated stemmed form — is in `tests/search.rs`. This half is the
/// configuration itself, because a tokenizer can only be set at
/// `CREATE VIRTUAL TABLE` time: changing it later means a new migration and a
/// full reindex, so it is worth failing loudly the moment it drifts.
#[test]
fn every_fts_index_uses_unicode61_without_stemming() {
    let db = open();
    for fts in FTS_TABLES {
        let sql = fts_ddl(db.conn(), fts);
        assert!(
            sql.contains("unicode61 remove_diacritics 2"),
            "`{fts}` does not use the §9.4 tokenizer: {sql}"
        );
        assert!(
            !sql.to_lowercase().contains("porter"),
            "`{fts}` stems; §9.4 forbids it on transcripts: {sql}"
        );
    }
}

/// §9.6, at the level a query can never see.
///
/// With `'secure-delete'` off — which is FTS5's default — retracting a row
/// appends a delete marker carrying the term it retracts, so deleting a
/// transcript writes a *second* copy of every one of its words into the file
/// while making the row instantly unfindable. Every behavioural test still
/// passes. The only two things that catch it are a byte scan (see
/// `src/delete.rs`) and this: the configuration itself, asserted directly out
/// of the `%_config` shadow table.
#[test]
fn every_fts_index_deletes_securely() {
    let db = open();
    for fts in FTS_TABLES {
        let v: Option<i64> = db
            .conn()
            .query_row(
                &format!("SELECT v FROM {fts}_config WHERE k = 'secure-delete'"),
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            v,
            Some(1),
            "`{fts}` retracts rows logically; its delete markers keep the words"
        );
    }
}

/// Nothing in SQLite connects a source table to an external-content index. The
/// triggers *are* the synchronisation, so their absence is a search result that
/// silently never appears — and, for the delete trigger, a transcript that
/// survives §9.6 in the index after its row is gone.
#[test]
fn every_fts_index_has_all_three_sync_triggers() {
    let db = open();
    let mut stmt = db
        .conn()
        .prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'trigger'")
        .unwrap();
    let triggers: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    for fts in FTS_TABLES {
        for (suffix, event) in [("ai", "INSERT"), ("au", "UPDATE"), ("ad", "DELETE")] {
            let name = format!("{fts}_{suffix}");
            let (_, sql) = triggers
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("missing trigger `{name}`"));
            assert!(
                sql.to_uppercase().contains(&format!("AFTER {event}")),
                "`{name}` is not an AFTER {event} trigger: {sql}"
            );
            // The only documented way to retract a row from an
            // external-content index. An ordinary DELETE against the FTS table
            // is an error, and simply not retracting leaves the tokens behind.
            if event != "INSERT" {
                assert!(
                    sql.contains(&format!("INSERT INTO {fts}({fts}, rowid,"))
                        && sql.contains("'delete'"),
                    "`{name}` does not use the documented 'delete' command form: {sql}"
                );
            }
        }
    }
}

/// STRICT is what makes the column types above mean anything. Without it
/// SQLite's type affinity happily stores the string '12' in an INTEGER column,
/// and the bug surfaces months later as a sort that puts '10' before '9'.
#[test]
fn every_table_is_strict() {
    let db = open();
    for (name, sql) in ddl(db.conn()) {
        assert!(
            sql.to_uppercase().contains("STRICT"),
            "table `{name}` is not STRICT"
        );
    }
}

/// §9.7 invariant 1. `AUTOINCREMENT` also drags in the `sqlite_sequence`
/// table, so its absence is a second, independent check on the same rule.
#[test]
fn no_autoincrement_anywhere() {
    let db = open();
    for (name, sql) in ddl(db.conn()) {
        assert!(
            !sql.to_uppercase().contains("AUTOINCREMENT"),
            "table `{name}` uses AUTOINCREMENT; §9.7 invariant 1 forbids it"
        );
    }
    let has_seq: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'sqlite_sequence'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_seq, 0);
}

/// Every primary key is TEXT, because every primary key holds a UUIDv7.
#[test]
fn every_primary_key_column_is_text() {
    let db = open();
    for table in EXPECTED_TABLES {
        let pks: Vec<_> = columns(db.conn(), table)
            .into_iter()
            .filter(|c| c.3 > 0)
            .collect();
        assert!(!pks.is_empty(), "table `{table}` has no primary key");
        for (name, ty, _, _) in pks {
            assert_eq!(ty, "TEXT", "`{table}.{name}` is a {ty} primary key");
        }
    }
}

/// Every timestamp is INTEGER epoch milliseconds. The naming convention is
/// half the enforcement — a column called `created_at` that is TEXT is an ISO
/// string somebody will eventually compare lexicographically against an
/// integer.
#[test]
fn every_timestamp_column_is_an_integer() {
    let db = open();
    for table in EXPECTED_TABLES {
        for (name, ty, _, _) in columns(db.conn(), table) {
            if name.ends_with("_at") || name.ends_with("_ms") {
                assert_eq!(
                    ty, "INTEGER",
                    "`{table}.{name}` looks like a timestamp but is {ty}"
                );
            }
        }
    }
}

/// §9.7 invariant 5: no absolute filesystem paths in the database, so the data
/// root can be moved or restored on another machine. The naming convention is
/// the enforceable half — a column named `*_rel_path` is a standing reminder
/// at every call site that joins it.
#[test]
fn every_path_column_is_named_relative() {
    let db = open();
    for table in EXPECTED_TABLES {
        for (name, _, _, _) in columns(db.conn(), table) {
            if name.contains("path") {
                assert!(
                    name.ends_with("_rel_path") || name == "rel_path",
                    "`{table}.{name}` is a path column that does not say it is relative"
                );
            }
        }
    }
}

/// §9.7 invariant 2. `updated_at` alone is not enough: merge order is
/// `(lamport, origin_device_id)`, never wall clock, because clock skew across
/// a user's two laptops is real and silent.
#[test]
fn every_mutable_table_carries_the_merge_triple() {
    // Tables whose rows are edited in place. The ones left out are left out
    // for a reason:
    //   segments / note_anchors  -- replaced wholesale with their parent
    //                               transcript or note, never edited
    //   summaries                -- append-only versions; carries created_at
    //                               and origin_device_id, and a new version is
    //                               how it changes
    //   tombstones               -- written once, never touched again
    //   app_meta                 -- local provenance, deliberately not synced
    //   devices                  -- the registry origin_device_id points AT
    const MUTABLE: &[&str] = &[
        "meetings",
        "folders",
        "templates",
        "people",
        "meeting_participants",
        "transcripts",
        "notes",
        "action_items",
        "tags",
        "recordings",
        "settings",
    ];

    let db = open();
    for table in MUTABLE {
        let cols = column_names(db.conn(), table);
        for required in ["updated_at", "lamport", "origin_device_id"] {
            assert!(
                cols.contains(required),
                "mutable table `{table}` is missing `{required}`"
            );
        }
    }
}

/// The single most important negative assertion in the schema.
///
/// A `meetings.summary_text` column is the obvious design and the wrong one:
/// it is one mutable cell that both devices would rewrite, which makes future
/// sync a merge nightmare, and regenerating a summary would destroy the
/// previous one with no way back.
#[test]
fn meetings_has_no_summary_text_column() {
    let db = open();
    let cols = column_names(db.conn(), "meetings");
    assert!(
        !cols.contains("summary_text"),
        "meetings.summary_text is forbidden; summaries are versioned rows"
    );
    assert!(
        !cols.iter().any(|c| c.contains("summary")),
        "no summary content may live on the meetings row: {cols:?}"
    );
}

/// §9.7 invariant 3: `notes.body_md` is the one deliberate exception to "no
/// column two devices would both rewrite", and must therefore stay the *only*
/// user-content column on that table — otherwise converting it to a CRDT later
/// means converting several things.
#[test]
fn notes_carries_exactly_one_content_column() {
    let db = open();
    let cols = column_names(db.conn(), "notes");
    let expected: BTreeSet<String> = [
        "id",
        "meeting_id",
        "body_md",
        "created_at",
        "updated_at",
        "lamport",
        "origin_device_id",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(
        cols, expected,
        "notes gained a column; body_md must remain the only content on this table"
    );
}

/// A tombstone remembers identity and destroys content. If a title, a snippet
/// or a path ever appears here, deleting a meeting stops meaning deleting it.
#[test]
fn tombstones_hold_identity_only() {
    let db = open();
    let cols = column_names(db.conn(), "tombstones");
    let expected: BTreeSet<String> = ["id", "kind", "deleted_at", "origin_device_id", "lamport"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert_eq!(cols, expected, "tombstones may only carry identity");
}

/// Word timings are a compressed blob per segment, not a row per word. A heavy
/// user produces ~8.4M words a year and no query ever wants one of them.
#[test]
fn segment_word_timings_are_a_blob() {
    let db = open();
    let ty = columns(db.conn(), "segments")
        .into_iter()
        .find(|c| c.0 == "words")
        .expect("segments.words must exist")
        .1;
    assert_eq!(ty, "BLOB", "word timings must be a zstd'd JSON blob");

    // And no per-word table sneaked in.
    assert!(!table_names(db.conn()).contains("words"));
}

/// The alignment column the augmented-notes feature is built on. NOT NULL
/// because an anchor without a time is not an anchor — it would silently point
/// the augmentation prompt at the whole meeting.
#[test]
fn note_anchors_records_when_each_block_was_typed() {
    let db = open();
    let (name, ty, notnull, _) = columns(db.conn(), "note_anchors")
        .into_iter()
        .find(|c| c.0 == "typed_at_ms")
        .expect("note_anchors.typed_at_ms must exist");
    assert_eq!(name, "typed_at_ms");
    assert_eq!(ty, "INTEGER");
    assert!(notnull, "typed_at_ms must be NOT NULL");
}

/// Multiple transcripts per meeting is the point — re-transcribing with a
/// different provider must not destroy the old one — and exactly one of them
/// being primary is a database invariant, not an application convention.
#[test]
fn a_meeting_may_have_many_transcripts_but_only_one_primary() {
    let db = open();
    let c = db.conn();
    c.execute(
        "INSERT INTO meetings (id, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
         VALUES ('m1', 0, 'UTC', 'ready', 0, 0, 'd1')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO transcripts (id, meeting_id, provider, model, is_primary, created_at, updated_at, origin_device_id)
         VALUES ('t1', 'm1', 'deepgram', 'nova-3', 1, 0, 0, 'd1')",
        [],
    )
    .unwrap();

    // A second, non-primary transcript is fine and is the whole feature.
    c.execute(
        "INSERT INTO transcripts (id, meeting_id, provider, model, is_primary, created_at, updated_at, origin_device_id)
         VALUES ('t2', 'm1', 'elevenlabs', 'scribe-v1', 0, 0, 0, 'd1')",
        [],
    )
    .unwrap();

    // A second primary is not.
    let err = c
        .execute(
            "INSERT INTO transcripts (id, meeting_id, provider, model, is_primary, created_at, updated_at, origin_device_id)
             VALUES ('t3', 'm1', 'openai', 'whisper-1', 1, 0, 0, 'd1')",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string().to_uppercase().contains("UNIQUE"),
        "transcripts_primary_uidx did not fire: {err}"
    );
}

/// The same shape for summaries: many versions, exactly one current.
#[test]
fn a_meeting_may_have_many_summaries_but_only_one_current() {
    let db = open();
    let c = db.conn();
    c.execute(
        "INSERT INTO meetings (id, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
         VALUES ('m1', 0, 'UTC', 'ready', 0, 0, 'd1')",
        [],
    )
    .unwrap();
    let insert = "INSERT INTO summaries
         (id, meeting_id, version, provider, model, prompt_hash, body_md, is_current, created_at, origin_device_id)
         VALUES (?1, 'm1', ?2, 'anthropic', 'claude', 'hash', 'body', ?3, 0, 'd1')";
    c.execute(insert, rusqlite::params!["s1", 1, 0]).unwrap();
    c.execute(insert, rusqlite::params!["s2", 2, 1]).unwrap();

    let err = c
        .execute(insert, rusqlite::params!["s3", 3, 1])
        .unwrap_err();
    assert!(
        err.to_string().to_uppercase().contains("UNIQUE"),
        "summaries_current_uidx did not fire: {err}"
    );
}

/// STRICT is only worth having if it actually rejects. This is the proof that
/// the tables really are strict, not merely annotated.
#[test]
fn strict_tables_reject_a_wrongly_typed_value() {
    let db = open();
    let err = db
        .conn()
        .execute(
            "INSERT INTO meetings (id, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
             VALUES ('m1', 'not-a-number', 'UTC', 'ready', 0, 0, 'd1')",
            [],
        )
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "cannot store TEXT value in INTEGER column meetings.started_at_ms",
        "expected a STRICT rejection"
    );
}

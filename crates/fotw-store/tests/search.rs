//! Full-text search acceptance tests (docs/REQUIREMENTS.md 9.4), including the
//! part of §9.6 that only becomes real once an FTS index exists.
//!
//! # The trap this file is written around
//!
//! Almost every assertion about a search index can be made vacuously. "Deleting
//! a meeting removes its transcript from the index" passes trivially against a
//! database with no index; "the tokenizer does not stem" passes trivially if
//! the fixture never got indexed at all. So most tests here carry a *control*:
//! a positive assertion, made against the same fixture, that fails if the index
//! is empty or the fixture is broken. A negative assertion with no control next
//! to it is not evidence of anything.

use std::time::Instant;

use fotw_store::{
    Db, FTS_TABLES, NewMeeting, NewSegment, NewSummary, NoteAnchor, SearchQuery, SearchSource,
    SearchWeights, StoreError,
};
use rusqlite::params;

mod common;
use common::{test_key, tmp_db};

fn db() -> Db {
    Db::open_in_memory(&test_key()).unwrap()
}

/// A meeting with a transcript whose segments are `lines`.
fn meeting_with_transcript(db: &mut Db, title: &str, lines: &[&str]) -> String {
    let id = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC").title(title))
        .unwrap();
    let t = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();
    let segments: Vec<NewSegment> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let i = i as i64;
            NewSegment::new(i, i * 5_000, i * 5_000 + 4_800, *line)
        })
        .collect();
    db.meetings().append_segments(&t, &segments).unwrap();
    id
}

/// Distinct terms an index holds for `term`, read out of the FTS index itself
/// through `fts5vocab` rather than out of the content table.
///
/// This is the only honest way to ask "does the index still hold this word?"
/// after the source row is gone: an external-content table answers
/// `SELECT ... FROM segments_fts` from `segments`, so once the meeting is
/// deleted that query returns nothing no matter what the index contains.
fn index_holds_term(db: &Db, fts_table: &str, term: &str) -> bool {
    let vocab = format!("v_{fts_table}");
    db.conn()
        .execute_batch(&format!(
            "DROP TABLE IF EXISTS temp.{vocab};
             CREATE VIRTUAL TABLE temp.{vocab} USING fts5vocab('main', '{fts_table}', 'row');"
        ))
        .unwrap();
    let n: i64 = db
        .conn()
        .query_row(
            &format!("SELECT count(*) FROM temp.{vocab} WHERE term = ?1"),
            params![term],
            |r| r.get(0),
        )
        .unwrap();
    db.conn()
        .execute_batch(&format!("DROP TABLE IF EXISTS temp.{vocab};"))
        .unwrap();
    n > 0
}

fn docs_indexed(db: &Db, fts_table: &str) -> i64 {
    db.search_index_document_counts()
        .unwrap()
        .into_iter()
        .find(|(t, _)| *t == fts_table)
        .unwrap()
        .1
}

// ---------------------------------------------------------------- the basics

/// The shape §9.4's acceptance criterion is written about: two terms, both
/// required, matched across the transcript body.
#[test]
fn a_two_term_query_requires_both_terms() {
    let mut db = db();
    let both = meeting_with_transcript(
        &mut db,
        "Platform sync",
        &["we should postpone the ingress cutover until the audit lands"],
    );
    let one = meeting_with_transcript(
        &mut db,
        "Design review",
        &["the ingress diagram needs another pass"],
    );

    let hits = db.search(&SearchQuery::new("ingress cutover")).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.meeting_id.as_str()).collect();
    assert_eq!(ids, [both.as_str()], "two terms means AND, not OR");

    // Control: the fixture really is indexed, so the exclusion above is the
    // AND doing its job rather than an empty index.
    let hits = db.search(&SearchQuery::new("ingress")).unwrap();
    assert_eq!(hits.len(), 2, "one term must match both meetings: {hits:?}");
    assert!(hits.iter().any(|h| h.meeting_id == one));
}

/// A hit points at the passage, not just the meeting: `snippet()` for the
/// preview and, for a transcript hit, the millisecond it was said at, so the UI
/// can seek the audio there.
#[test]
fn a_hit_carries_a_marked_snippet_and_a_transcript_hit_carries_its_timestamp() {
    let mut db = db();
    let id = meeting_with_transcript(
        &mut db,
        "Platform sync",
        &[
            "good morning everyone thanks for joining today",
            "the important part is that we postpone the ingress cutover",
        ],
    );

    let hits = db.search(&SearchQuery::new("cutover")).unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit.meeting_id, id);
    assert_eq!(hit.source, SearchSource::Transcript);
    assert!(
        hit.snippet.contains("[cutover]"),
        "snippet() must mark the match: {:?}",
        hit.snippet
    );
    assert_eq!(
        hit.start_ms,
        Some(5_000),
        "a transcript hit is positioned in time so the UI can seek to it"
    );
    assert!(
        hit.score < 0.0,
        "bm25() is negative; more negative is better"
    );

    // A title hit is not positioned in time, and says so rather than lying
    // with a zero.
    let hits = db.search(&SearchQuery::new("Platform")).unwrap();
    assert_eq!(hits[0].source, SearchSource::Title);
    assert_eq!(hits[0].start_ms, None);
}

/// An empty `MATCH` is a syntax error in FTS5 and "everything" is not a
/// defensible reading of "". Refusing by name beats a raw SQLite error the UI
/// cannot classify.
#[test]
fn a_query_with_no_searchable_word_is_refused_by_name() {
    let mut db = db();
    meeting_with_transcript(&mut db, "Sync", &["hello"]);
    let err = db.search(&SearchQuery::new("   ??? ")).unwrap_err();
    assert!(matches!(err, StoreError::InvalidArgument(_)), "got {err:?}");
}

/// FTS5's query language is not the user's. None of these may reach `MATCH` as
/// syntax — the failure mode is an error dialog for a perfectly ordinary
/// search box entry.
#[test]
fn fts5_operator_syntax_typed_by_a_user_is_treated_as_words() {
    let mut db = db();
    meeting_with_transcript(&mut db, "Sync", &["the C compiler and the parser"]);

    for input in ["C++", "\"unbalanced", "NEAR(a b)", "a AND b", "parser OR"] {
        let hits = db.search(&SearchQuery::new(input));
        assert!(
            hits.is_ok(),
            "user input {input:?} reached MATCH as syntax: {:?}",
            hits.err()
        );
    }
}

// ------------------------------------------------------------- the tokenizer

/// §9.4: `unicode61`, deliberately **not** `porter`.
///
/// Porter maps "Universal" and "universe" onto the same stem (`univers`), so a
/// stemming index would answer a search for "universe" with every meeting that
/// mentioned Universal Analytics. Product names and acronyms are most of what
/// people search meetings for, and conflating them is the specific harm §9.4
/// trades recall away to avoid.
#[test]
fn the_tokenizer_does_not_stem_so_a_product_name_does_not_match_an_unrelated_word() {
    let mut db = db();
    let id = meeting_with_transcript(
        &mut db,
        "Analytics migration",
        &[
            "the Universal Analytics property finally stops collecting in July",
            "the operator on call should watch the caching layer",
        ],
    );

    // Control first: the fixture is indexed and findable by its real name.
    let hits = db.search(&SearchQuery::new("Universal")).unwrap();
    assert!(
        hits.iter().any(|h| h.meeting_id == id),
        "fixture is not indexed at all, so the assertions below prove nothing"
    );

    // Every pair here is one Porter conflates and unicode61 does not.
    for (typed, stemmed_into) in [
        ("universe", "Universal"),
        ("operating", "operator"),
        ("caches", "caching"),
    ] {
        let hits = db.search(&SearchQuery::new(typed)).unwrap();
        assert!(
            hits.is_empty(),
            "searching {typed:?} matched {stemmed_into:?}; the index is stemming: {hits:?}"
        );
    }
}

/// The other half of the tokenizer setting. `remove_diacritics 2` is the fixed
/// variant — 1 misses codepoints carrying more than one diacritic — and it is
/// what lets somebody type "Zurich" and find the meeting where they wrote
/// "Zürich".
#[test]
fn diacritics_are_folded_in_both_directions() {
    let mut db = db();
    meeting_with_transcript(
        &mut db,
        "Offsite",
        &["the Zürich offsite moves to the résumé of the quarter"],
    );

    for typed in ["Zurich", "Zürich", "resume", "résumé"] {
        let hits = db.search(&SearchQuery::new(typed)).unwrap();
        assert_eq!(hits.len(), 1, "searching {typed:?} found {hits:?}");
    }
}

// ---------------------------------------------------------------- the ranking

/// Build a corpus where one meeting holds the term in its title, another in its
/// note, another in its summary and another only in its transcript — with
/// enough uninteresting documents around them that the term is genuinely rare
/// in all four indexes.
///
/// The filler is not decoration. `bm25()` scores a term by its inverse document
/// frequency *within its own index*, and clamps the IDF to ~0 for a term that
/// occurs in more than half the documents. With three documents in the corpus,
/// every score collapses to the same near-zero number and a ranking test
/// becomes a coin toss dressed as an assertion.
fn ranking_corpus(db: &mut Db, term: &str) -> [String; 4] {
    const FILLER: &str = "the roadmap item was discussed and then the owner took \
                          the follow up away for the next cycle";

    let mut ids = Vec::new();
    for i in 0..60i64 {
        let title = match i {
            0 => format!("{term} rollout"),
            _ => format!("weekly sync number {i}"),
        };
        let id = db
            .meetings()
            .create(
                NewMeeting::new("dev-1", "UTC")
                    .title(title)
                    .started_at_ms(1_700_000_000_000 + i * 86_400_000),
            )
            .unwrap();

        let note = if i == 1 {
            format!("we agreed the {term} work is blocked on the audit")
        } else {
            format!("{FILLER} {i}")
        };
        db.meetings()
            .upsert_note(&id, &note, &[NoteAnchor::new(0, &note, 1_000)])
            .unwrap();

        let summary = if i == 2 {
            format!("the {term} plan slipped by a week and needs a new owner")
        } else {
            format!("{FILLER} summarised for meeting {i}")
        };
        db.meetings()
            .insert_summary(
                &id,
                NewSummary::new("dev-1", "anthropic", "claude", "hash", summary),
            )
            .unwrap();

        let t = db
            .meetings()
            .create_transcript(&id, "deepgram", "nova-3", true)
            .unwrap();
        let segments: Vec<NewSegment> = (0..6i64)
            .map(|j| {
                let text = if i == 3 && j == 3 {
                    format!("and then somebody mentioned the {term} thing in passing here")
                } else {
                    format!("{FILLER} at point {j}")
                };
                NewSegment::new(j, j * 5_000, j * 5_000 + 4_800, text)
            })
            .collect();
        db.meetings().append_segments(&t, &segments).unwrap();

        ids.push(id);
    }

    [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ]
}

/// §9.4: "weight titles and notes above transcript body".
#[test]
fn titles_and_notes_outrank_summaries_and_transcript_body() {
    let mut db = db();
    let [by_title, by_note, by_summary, by_transcript] = ranking_corpus(&mut db, "falcon");

    let hits = db.search(&SearchQuery::new("falcon")).unwrap();
    let order: Vec<(&str, SearchSource)> = hits
        .iter()
        .map(|h| (h.meeting_id.as_str(), h.source))
        .collect();

    assert_eq!(
        order,
        [
            (by_title.as_str(), SearchSource::Title),
            (by_note.as_str(), SearchSource::Note),
            (by_summary.as_str(), SearchSource::Summary),
            (by_transcript.as_str(), SearchSource::Transcript),
        ],
        "ranking is wrong; scores were {:?}",
        hits.iter().map(|h| h.score).collect::<Vec<_>>()
    );
}

/// The weights are the mechanism, not a happy accident of the corpus. Inverting
/// them has to invert the result — otherwise the ordering above was coming from
/// somewhere else and `SearchWeights` is decoration.
#[test]
fn inverting_the_weights_inverts_the_ranking() {
    let mut db = db();
    let [by_title, _, _, by_transcript] = ranking_corpus(&mut db, "falcon");

    let inverted = SearchWeights {
        title: 1.0,
        note: 1.0,
        summary: 1.0,
        transcript: 64.0,
    };
    let hits = db
        .search(&SearchQuery::new("falcon").weights(inverted))
        .unwrap();

    assert_eq!(hits[0].meeting_id, by_transcript);
    assert_eq!(hits[0].source, SearchSource::Transcript);
    assert!(
        hits.iter().position(|h| h.meeting_id == by_title).unwrap() > 0,
        "the title hit should no longer lead: {hits:?}"
    );
}

/// Paging happens after ranking and after filtering, in SQL. Doing it in Rust
/// would silently return short pages the moment a filter is active.
#[test]
fn results_page_without_gaps_or_repeats() {
    let mut db = db();
    ranking_corpus(&mut db, "falcon");

    let all = db.search(&SearchQuery::new("falcon")).unwrap();
    assert_eq!(all.len(), 4);

    let first = db.search(&SearchQuery::new("falcon").limit(2)).unwrap();
    let second = db
        .search(&SearchQuery::new("falcon").limit(2).offset(2))
        .unwrap();
    let paged: Vec<_> = first.iter().chain(second.iter()).cloned().collect();
    assert_eq!(
        paged, all,
        "paging must reproduce the unpaged order exactly"
    );
}

// ----------------------------------------------------------------- the filters

/// A fixture with folders, tags, participants and spread-out dates, so each
/// filter has something to exclude.
fn filter_corpus(db: &mut Db) -> Vec<String> {
    db.conn()
        .execute_batch(
            "INSERT INTO folders (id, name, created_at, updated_at, origin_device_id)
                  VALUES ('f-work', 'Work', 0, 0, 'dev-1'), ('f-home', 'Home', 0, 0, 'dev-1');
             INSERT INTO tags (id, name, created_at, updated_at, origin_device_id)
                  VALUES ('t-q3', 'quarterly', 0, 0, 'dev-1'), ('t-adhoc', 'adhoc', 0, 0, 'dev-1');",
        )
        .unwrap();

    let mut ids = Vec::new();
    for (i, (folder, tag, person, day)) in [
        ("f-work", "t-q3", "Ada Lovelace", 0i64),
        ("f-work", "t-adhoc", "Grace Hopper", 10),
        ("f-home", "t-q3", "Ada Lovelace", 20),
        ("f-home", "t-adhoc", "Grace Hopper", 30),
    ]
    .into_iter()
    .enumerate()
    {
        let id = db
            .meetings()
            .create(
                NewMeeting::new("dev-1", "UTC")
                    .title(format!("meeting {i}"))
                    .started_at_ms(1_700_000_000_000 + day * 86_400_000),
            )
            .unwrap();
        let t = db
            .meetings()
            .create_transcript(&id, "deepgram", "nova-3", true)
            .unwrap();
        db.meetings()
            .append_segments(
                &t,
                &[NewSegment::new(
                    0,
                    0,
                    1_000,
                    "everyone agreed the telemetry rollout should go ahead",
                )],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE meetings SET folder_id = ?2 WHERE id = ?1",
                params![id, folder],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO meeting_tags (meeting_id, tag_id, created_at, origin_device_id)
                 VALUES (?1, ?2, 0, 'dev-1')",
                params![id, tag],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO meeting_participants
                     (id, meeting_id, display_name, email, created_at, updated_at, origin_device_id)
                 VALUES (?1, ?2, ?3, ?4, 0, 0, 'dev-1')",
                params![
                    format!("p-{i}"),
                    id,
                    person,
                    format!(
                        "{}@example.com",
                        person.split(' ').next().unwrap().to_lowercase()
                    )
                ],
            )
            .unwrap();
        ids.push(id);
    }
    ids
}

#[test]
fn each_filter_narrows_the_same_text_query() {
    let mut db = db();
    let ids = filter_corpus(&mut db);

    let unfiltered = db.search(&SearchQuery::new("telemetry rollout")).unwrap();
    assert_eq!(unfiltered.len(), 4, "control: all four match the text");

    let by_folder = db
        .search(&SearchQuery::new("telemetry rollout").folder("f-work"))
        .unwrap();
    assert_eq!(hit_ids(&by_folder), [&ids[0], &ids[1]]);

    let by_tag = db
        .search(&SearchQuery::new("telemetry rollout").tag("quarterly"))
        .unwrap();
    assert_eq!(hit_ids(&by_tag), [&ids[0], &ids[2]]);

    let by_name = db
        .search(&SearchQuery::new("telemetry rollout").participant("grace hopper"))
        .unwrap();
    assert_eq!(
        hit_ids(&by_name),
        [&ids[1], &ids[3]],
        "participant match is case-insensitive on the display name"
    );

    let by_email = db
        .search(&SearchQuery::new("telemetry rollout").participant("ADA@example.com"))
        .unwrap();
    assert_eq!(hit_ids(&by_email), [&ids[0], &ids[2]]);

    let by_date = db
        .search(&SearchQuery::new("telemetry rollout").between(
            1_700_000_000_000 + 5 * 86_400_000,
            1_700_000_000_000 + 25 * 86_400_000,
        ))
        .unwrap();
    assert_eq!(hit_ids(&by_date), [&ids[1], &ids[2]]);
}

/// Text and every filter in one call. Applying them in separate passes is what
/// breaks paging, and "no results" is the only observable difference between a
/// correctly combined query and four filters ANDed in the wrong scope.
#[test]
fn text_and_all_four_filters_combine_in_one_call() {
    let mut db = db();
    let ids = filter_corpus(&mut db);

    let q = SearchQuery::new("telemetry rollout")
        .folder("f-work")
        .tag("quarterly")
        .participant("Ada Lovelace")
        .between(1_699_000_000_000, 1_800_000_000_000);
    assert_eq!(hit_ids(&db.search(&q).unwrap()), [&ids[0]]);

    // Flip one filter to a value the same meeting does not satisfy: the
    // combination must be an AND, not a union.
    let q = q.tag("adhoc");
    assert!(db.search(&q).unwrap().is_empty());
}

fn hit_ids(hits: &[fotw_store::SearchHit]) -> Vec<&String> {
    let mut ids: Vec<&String> = hits.iter().map(|h| &h.meeting_id).collect();
    ids.sort();
    ids.dedup();
    ids
}

// ---------------------------------------------------------- keeping in sync

/// The UPDATE half of the trigger set. An external-content index does not
/// notice an edit; without the trigger, a note keeps matching the words it used
/// to contain and stops matching the words it now does.
#[test]
fn editing_a_note_a_title_or_a_summary_reindexes_it() {
    let mut db = db();
    let id = meeting_with_transcript(&mut db, "Draft title", &["nothing interesting here"]);
    db.meetings()
        .upsert_note(&id, "the original wording", &[])
        .unwrap();

    assert_eq!(db.search(&SearchQuery::new("original")).unwrap().len(), 1);
    assert_eq!(db.search(&SearchQuery::new("Draft")).unwrap().len(), 1);

    db.meetings()
        .upsert_note(&id, "the replacement wording", &[])
        .unwrap();
    db.conn()
        .execute(
            "UPDATE meetings SET title = 'Published title' WHERE id = ?1",
            params![id],
        )
        .unwrap();

    assert_eq!(
        db.search(&SearchQuery::new("replacement")).unwrap().len(),
        1
    );
    assert_eq!(db.search(&SearchQuery::new("Published")).unwrap().len(), 1);
    assert!(
        db.search(&SearchQuery::new("original")).unwrap().is_empty(),
        "the superseded note text is still matching"
    );
    assert!(
        db.search(&SearchQuery::new("Draft")).unwrap().is_empty(),
        "the superseded title is still matching"
    );
    db.verify_search_index()
        .expect("an update must leave the index consistent with its content table");
}

/// Re-transcribing a meeting deletes the old transcript's segments through a
/// cascade that starts one table lower than §9.6's. The tokens have to go with
/// them, or the old provider's mistakes keep turning up in search forever.
#[test]
fn deleting_a_transcript_retracts_its_segments_from_the_index() {
    let mut db = db();
    let id = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC").title("Sync"))
        .unwrap();
    let old = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &old,
            &[NewSegment::new(0, 0, 1_000, "the wobblegong was misheard")],
        )
        .unwrap();
    assert_eq!(db.search(&SearchQuery::new("wobblegong")).unwrap().len(), 1);

    db.conn()
        .execute("DELETE FROM transcripts WHERE id = ?1", params![old])
        .unwrap();

    assert!(
        db.search(&SearchQuery::new("wobblegong"))
            .unwrap()
            .is_empty()
    );
    assert!(
        !index_holds_term(&db, "segments_fts", "wobblegong"),
        "the token survived in the index after its segment was cascaded away"
    );
    db.verify_search_index().unwrap();
}

// ------------------------------------------------------------ search:rebuild

/// §9.7 invariant 6: every derived index is reconstructible from the source
/// tables alone, by one command.
///
/// The index is destroyed with FTS5's own `'delete-all'` — a real emptying of
/// the shadow tables, not a `DROP` that a re-migration would paper over — and
/// then rebuilt with nothing but the content tables to go on.
#[test]
fn clearing_every_index_and_rebuilding_restores_every_result() {
    let mut db = db();
    let ids = filter_corpus(&mut db);
    db.meetings()
        .upsert_note(&ids[0], "the flywheel note", &[])
        .unwrap();
    db.meetings()
        .insert_summary(
            &ids[0],
            NewSummary::new("dev-1", "anthropic", "claude", "h", "the flywheel summary"),
        )
        .unwrap();

    let before = db.search(&SearchQuery::new("telemetry rollout")).unwrap();
    let before_flywheel = db.search(&SearchQuery::new("flywheel")).unwrap();
    assert_eq!(before.len(), 4);
    assert_eq!(before_flywheel.len(), 2, "one note hit and one summary hit");
    let counts_before = db.search_index_document_counts().unwrap();
    assert!(counts_before.iter().all(|(_, n)| *n > 0));

    for table in FTS_TABLES {
        db.conn()
            .execute_batch(&format!(
                "INSERT INTO {table}({table}) VALUES('delete-all');"
            ))
            .unwrap();
    }

    // Control: the corruption is real. Without this the rebuild below could be
    // restoring an index that was never damaged.
    assert!(
        db.search_index_document_counts()
            .unwrap()
            .iter()
            .all(|(_, n)| *n == 0),
        "delete-all did not empty the shadow tables"
    );
    assert!(
        db.search(&SearchQuery::new("telemetry rollout"))
            .unwrap()
            .is_empty()
    );
    assert!(db.search(&SearchQuery::new("flywheel")).unwrap().is_empty());

    db.rebuild_search_index().unwrap();

    assert_eq!(
        db.search(&SearchQuery::new("telemetry rollout")).unwrap(),
        before,
        "rebuild must restore the results, ranking and snippets exactly"
    );
    assert_eq!(
        db.search(&SearchQuery::new("flywheel")).unwrap(),
        before_flywheel
    );
    assert_eq!(db.search_index_document_counts().unwrap(), counts_before);
    db.verify_search_index().unwrap();
}

/// Rebuilding a healthy index is a no-op, so `search:rebuild` is safe to offer
/// as a "fix my search" button rather than as a support-only escape hatch.
#[test]
fn rebuilding_a_healthy_index_changes_nothing() {
    let mut db = db();
    filter_corpus(&mut db);
    let before = db.search(&SearchQuery::new("telemetry")).unwrap();
    db.rebuild_search_index().unwrap();
    assert_eq!(db.search(&SearchQuery::new("telemetry")).unwrap(), before);
    db.verify_search_index().unwrap();
}

/// Compaction is a maintenance operation, so the one thing it must never do is
/// change an answer.
#[test]
fn optimizing_the_index_changes_no_result() {
    let mut db = db();
    let ids = filter_corpus(&mut db);
    db.meetings()
        .upsert_note(&ids[0], "the flywheel note", &[])
        .unwrap();
    // Churn first: an index with a single b-tree has nothing to merge, so
    // optimizing it would be a no-op and this would prove nothing.
    for body in ["draft one", "draft two", "the flywheel note again"] {
        db.meetings().upsert_note(&ids[1], body, &[]).unwrap();
    }

    let before = db.search(&SearchQuery::new("flywheel")).unwrap();
    assert_eq!(before.len(), 2);

    db.optimize_search_index().unwrap();

    assert_eq!(db.search(&SearchQuery::new("flywheel")).unwrap(), before);
    assert!(
        db.search(&SearchQuery::new("draft")).unwrap().is_empty(),
        "superseded note drafts must not come back"
    );
    db.verify_search_index().unwrap();
}

/// `verify_search_index` has to be able to *fail*, or every place it is called
/// as a positive assertion is decoration.
#[test]
fn the_integrity_check_notices_an_index_that_disagrees_with_its_content_table() {
    let mut db = db();
    filter_corpus(&mut db);
    db.verify_search_index()
        .expect("control: a fresh index is consistent");

    db.conn()
        .execute_batch("INSERT INTO segments_fts(segments_fts) VALUES('delete-all');")
        .unwrap();

    db.verify_search_index()
        .expect_err("an emptied index over a populated content table must not verify");

    db.rebuild_search_index().unwrap();
    db.verify_search_index().unwrap();
}

// ------------------------------------------------- §9.6, no longer vacuously

/// §9.6, the clause that only became real with migration 0002.
///
/// "Deleting a meeting leaves no fragment of its transcript anywhere" was
/// satisfied *vacuously* before this migration, because there was no FTS index
/// to hold anything. There is now, and an external-content index keeps every
/// token after the source row is deleted unless something retracts it. This
/// asserts the retraction three independent ways: through the public search
/// API, through the `%_docsize` shadow table, and through `fts5vocab`, which
/// reads the index's own term list and cannot be fooled by an empty content
/// table.
#[test]
fn deleting_a_meeting_removes_its_words_from_every_index() {
    // One token, not a hyphenated phrase: the index stores tokens, so a
    // multi-token needle would be absent from it for reasons that have nothing
    // to do with the delete working.
    const NEEDLE: &str = "zarquonfizzbinquantumhedgehog";

    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();

    let id = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC").title(format!("The {NEEDLE} review")))
        .unwrap();
    let t = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();
    let segments: Vec<NewSegment> = (0..64i64)
        .map(|i| {
            NewSegment::new(
                i,
                i * 1_000,
                i * 1_000 + 900,
                format!("line {i} about the {NEEDLE} and its consequences"),
            )
        })
        .collect();
    db.meetings().append_segments(&t, &segments).unwrap();
    db.meetings()
        .upsert_note(&id, &format!("- {NEEDLE} is blocked"), &[])
        .unwrap();
    db.meetings()
        .insert_summary(
            &id,
            NewSummary::new(
                "dev-1",
                "anthropic",
                "claude",
                "h",
                format!("the {NEEDLE} slipped"),
            ),
        )
        .unwrap();

    // Control. Every one of the four indexes holds the needle, so every
    // assertion after the delete is about the delete.
    let before = db.search(&SearchQuery::new(NEEDLE)).unwrap();
    let sources: std::collections::BTreeSet<&str> =
        before.iter().map(|h| h.source.as_str()).collect();
    assert_eq!(
        sources,
        ["note", "summary", "title", "transcript"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        "fixture must reach all four indexes: {before:?}"
    );
    for table in FTS_TABLES {
        assert!(
            index_holds_term(&db, table, NEEDLE),
            "fixture: `{table}` should hold the needle before the delete"
        );
    }

    db.delete_meeting(&id).unwrap();

    assert!(
        db.search(&SearchQuery::new(NEEDLE)).unwrap().is_empty(),
        "the transcript is still findable after its meeting was deleted"
    );
    for table in FTS_TABLES {
        assert!(
            !index_holds_term(&db, table, NEEDLE),
            "`{table}` still holds the deleted meeting's words in its index"
        );
        assert_eq!(
            docs_indexed(&db, table),
            0,
            "`{table}_docsize` still has rows for the deleted meeting"
        );
    }
    db.verify_search_index()
        .expect("the index must still agree with the (now empty) content tables");

    drop(db);
    drop(dir);
}

// The byte-scan half of this property lives in `src/delete.rs`'s unit tests, not
// here. It has to: §9.6's correction requires it to run against a *plaintext*
// database (grepping a SQLCipher file for a plaintext needle passes against an
// implementation that deletes nothing), and the plaintext opener is a
// `cfg(test)`-only, crate-private constructor precisely so that an unencrypted
// meeting library cannot be produced by shipping code.

// ------------------------------------------------------------- performance

/// Meetings in the §9.4 acceptance corpus.
const PERF_MEETINGS: i64 = 1_250;
/// Transcript segments per meeting.
const PERF_SEGMENTS: i64 = 120;
/// §9.4's p95 budget for a two-term query.
const PERF_BUDGET_MS: u128 = 100;

/// §9.4: under 100 ms p95 for a two-term query over 1,250 meetings.
///
/// The corpus is generated rather than fixed so the numbers move with the
/// schema. Note that this runs in a `cargo test` (dev) profile, which compiles
/// the vendored SQLCipher with `-O0`; the shipped build is `-O3` with LTO, so
/// this is a pessimistic measurement, not an optimistic one.
#[test]
fn a_two_term_query_over_the_acceptance_corpus_stays_under_the_budget() {
    let mut db = db();
    let built = Instant::now();
    let corpus_terms = build_perf_corpus(&mut db);
    let build_ms = built.elapsed().as_millis();

    let meetings: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM meetings", [], |r| r.get(0))
        .unwrap();
    let segments: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM segments", [], |r| r.get(0))
        .unwrap();
    assert_eq!(meetings, PERF_MEETINGS);

    // Warm the page cache and the prepared-statement cache the way a running
    // app's would be; measuring the first query would be measuring startup.
    for pair in &corpus_terms {
        db.search(&SearchQuery::new(pair.clone()).limit(50))
            .unwrap();
    }

    let mut samples: Vec<u128> = Vec::new();
    let mut total_hits = 0usize;
    for _ in 0..4 {
        for pair in &corpus_terms {
            let t = Instant::now();
            let hits = db
                .search(&SearchQuery::new(pair.clone()).limit(50))
                .unwrap();
            samples.push(t.elapsed().as_micros());
            total_hits += hits.len();
        }
    }
    assert!(
        total_hits > 0,
        "the perf corpus returned no hits at all, so nothing was measured"
    );

    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95).div_ceil(100).min(samples.len()) - 1];
    let p50 = samples[samples.len() / 2];
    let worst = *samples.last().unwrap();

    println!(
        "search over {meetings} meetings / {segments} segments: \
         p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms ({} samples, {total_hits} hits, \
         corpus built in {build_ms} ms)",
        p50 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        worst as f64 / 1000.0,
        samples.len(),
    );

    assert!(
        p95 / 1000 < PERF_BUDGET_MS,
        "§9.4 budget is {PERF_BUDGET_MS} ms p95; measured {:.1} ms (p50 {:.1} ms, max {:.1} ms)",
        p95 as f64 / 1000.0,
        p50 as f64 / 1000.0,
        worst as f64 / 1000.0,
    );
}

/// 1,250 meetings, each with a title, a note, a summary and a transcript.
///
/// Built with raw batched SQL rather than through `MeetingRepo` because the
/// repo commits per call and 1,250 × 4 fsyncs is the test, not the search.
/// Returns the two-term queries to measure with.
fn build_perf_corpus(db: &mut Db) -> Vec<String> {
    // A small Zipf-ish vocabulary: a handful of words in most segments, a long
    // tail in few. A uniform vocabulary would make every term equally selective
    // and quietly measure the easy case.
    const COMMON: &[&str] = &[
        "the", "we", "should", "that", "team", "week", "will", "need", "release", "ship",
    ];
    const MID: &[&str] = &[
        "ingress",
        "cutover",
        "latency",
        "retention",
        "billing",
        "onboarding",
        "rollback",
        "telemetry",
        "quota",
        "migration",
        "dashboard",
        "throughput",
        "incident",
        "staging",
    ];
    const RARE: &[&str] = &[
        "wobblegong",
        "flywheel",
        "zarquon",
        "pergola",
        "quokka",
        "basalt",
    ];

    let tx = db.conn_mut().transaction().unwrap();
    {
        let mut meeting = tx
            .prepare(
                "INSERT INTO meetings (id, title, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
                 VALUES (?1, ?2, ?3, 'UTC', 'ready', ?3, ?3, 'dev-1')",
            )
            .unwrap();
        let mut transcript = tx
            .prepare(
                "INSERT INTO transcripts (id, meeting_id, provider, model, created_at, updated_at, origin_device_id)
                 VALUES (?1, ?2, 'deepgram', 'nova-3', 0, 0, 'dev-1')",
            )
            .unwrap();
        let mut note = tx
            .prepare(
                "INSERT INTO notes (id, meeting_id, body_md, created_at, updated_at, origin_device_id)
                 VALUES (?1, ?2, ?3, 0, 0, 'dev-1')",
            )
            .unwrap();
        let mut summary = tx
            .prepare(
                "INSERT INTO summaries (id, meeting_id, version, provider, model, prompt_hash, body_md, is_current, created_at, origin_device_id)
                 VALUES (?1, ?2, 1, 'anthropic', 'claude', 'h', ?3, 1, 0, 'dev-1')",
            )
            .unwrap();
        let mut segment = tx
            .prepare(
                "INSERT INTO segments (id, transcript_id, meeting_id, idx, start_ms, end_ms, channel, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'mic', ?7)",
            )
            .unwrap();

        // A cheap deterministic PRNG: a fixed corpus means a failing p95 can be
        // reproduced, and no dev-dependency on `rand`.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move |n: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };

        for m in 0..PERF_MEETINGS {
            let mid = format!("m{m:06}");
            let tid = format!("t{m:06}");
            let started = 1_600_000_000_000 + m * 3_600_000;
            meeting
                .execute(params![
                    mid,
                    format!("{} sync {m}", MID[next(MID.len())]),
                    started
                ])
                .unwrap();
            transcript.execute(params![tid, mid]).unwrap();
            note.execute(params![
                format!("n{m:06}"),
                mid,
                format!(
                    "- {} follow up\n- {} owner tbd",
                    MID[next(MID.len())],
                    MID[next(MID.len())]
                )
            ])
            .unwrap();
            summary
                .execute(params![
                    format!("su{m:06}"),
                    mid,
                    format!(
                        "The team discussed {} and agreed to revisit {}.",
                        MID[next(MID.len())],
                        MID[next(MID.len())]
                    )
                ])
                .unwrap();

            for s in 0..PERF_SEGMENTS {
                let mut words = String::with_capacity(96);
                for w in 0..14 {
                    if w > 0 {
                        words.push(' ');
                    }
                    let roll = next(100);
                    if roll < 70 {
                        words.push_str(COMMON[next(COMMON.len())]);
                    } else if roll < 99 {
                        words.push_str(MID[next(MID.len())]);
                    } else {
                        words.push_str(RARE[next(RARE.len())]);
                    }
                }
                segment
                    .execute(params![
                        format!("s{m:06}-{s:04}"),
                        tid,
                        mid,
                        s,
                        s * 5_000,
                        s * 5_000 + 4_800,
                        words
                    ])
                    .unwrap();
            }
        }
    }
    tx.commit().unwrap();

    // Two-term queries across the selectivity range a user actually types.
    vec![
        "ingress cutover".to_owned(),
        "billing retention".to_owned(),
        "telemetry rollback".to_owned(),
        "wobblegong flywheel".to_owned(),
        "quota staging".to_owned(),
        "incident dashboard".to_owned(),
    ]
}

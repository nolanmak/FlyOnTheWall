//! Full-text search over the library (docs/REQUIREMENTS.md 9.4).
//!
//! The four FTS5 indexes live in migration 0002; this module is the read side
//! and the two operations that keep it honest — [`Db::search`] and
//! [`Db::rebuild_search_index`].
//!
//! # Why the query is built here and not by the caller
//!
//! `MATCH` takes an expression in FTS5's own query language, not a string of
//! words. `C++` is a syntax error, a stray `"` is a syntax error, and `AND` and
//! `NEAR` are keywords — so handing user input straight to `MATCH` turns a
//! search box into a source of errors at best and of surprises at worst.
//! [`to_match_expression`] reduces the input to the same tokens `unicode61`
//! would produce and quotes each one, which makes every possible input a legal
//! query.

use rusqlite::{OptionalExtension, named_params};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::{Result, StoreError};

/// The four FTS5 indexes, in the order `search:rebuild` visits them.
///
/// Public because the schema lint in `tests/schema.rs` needs to know which
/// tables are exempt from the STRICT rule, and duplicating the list there is
/// how it would drift.
pub const FTS_TABLES: [&str; 4] = ["meetings_fts", "notes_fts", "summaries_fts", "segments_fts"];

/// How many tokens of context [`snippet`](https://sqlite.org/fts5.html#the_snippet_function)
/// puts around a hit.
const SNIPPET_TOKENS: i64 = 24;
/// Marks the matched terms inside a snippet.
const SNIPPET_OPEN: &str = "[";
/// Closes [`SNIPPET_OPEN`].
const SNIPPET_CLOSE: &str = "]";
/// Stands in for the text a snippet dropped.
const SNIPPET_ELLIPSIS: &str = "…";

/// Which of the four indexes a hit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSource {
    /// `meetings.title`.
    Title,
    /// `notes.body_md` — what the user typed during the meeting.
    Note,
    /// `summaries.body_md`, any version, not only the current one.
    Summary,
    /// `segments.text` — the transcript body.
    Transcript,
}

impl SearchSource {
    /// The literal this source is tagged with inside the SQL union.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Note => "note",
            Self::Summary => "summary",
            Self::Transcript => "transcript",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "title" => Some(Self::Title),
            "note" => Some(Self::Note),
            "summary" => Some(Self::Summary),
            "transcript" => Some(Self::Transcript),
            _ => None,
        }
    }
}

/// Per-source ranking weights: §9.4's "weight titles and notes above transcript
/// body".
///
/// # Why this is not just a `bm25()` column weight
///
/// `bm25()`'s trailing arguments *are* column weights, and they are passed (see
/// [`Db::search`]) — but they can only reweight columns *within one index*, and
/// each of these four indexes has a single content column. Worse, the four are
/// four different corpora: a term that is rare among 200,000 transcript
/// segments has a much larger IDF than the same term among 1,250 meeting
/// titles, so the raw `bm25()` values are not on a common scale to begin with.
///
/// So the cross-source weighting is applied where it is meaningful: as a
/// multiplier on each index's own score. `bm25()` returns a *negative* number
/// whose magnitude grows with relevance, so multiplying by a larger weight
/// moves a hit further to the front of an `ORDER BY score ASC`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchWeights {
    /// Weight for `meetings.title` hits.
    pub title: f64,
    /// Weight for `notes.body_md` hits.
    pub note: f64,
    /// Weight for `summaries.body_md` hits.
    pub summary: f64,
    /// Weight for `segments.text` hits.
    pub transcript: f64,
}

impl Default for SearchWeights {
    /// Title > note > summary > transcript.
    ///
    /// A title is a deliberate human label and is nearly always the strongest
    /// signal there is; notes are the user's own words about what mattered; a
    /// summary is a machine's words about the same thing; the transcript is
    /// everything anybody said, including the small talk.
    fn default() -> Self {
        Self {
            title: 8.0,
            note: 4.0,
            summary: 2.0,
            transcript: 1.0,
        }
    }
}

/// A search, with its filters.
///
/// Text and filters are one call rather than a text search followed by
/// filtering in Rust: `LIMIT` has to be applied *after* both, or paging through
/// "meetings in this folder mentioning X" silently returns short pages.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// Raw user input. Reduced to tokens; FTS5 operators are not honoured.
    pub text: String,
    /// Maximum hits to return.
    pub limit: i64,
    /// Hits to skip, for paging.
    pub offset: i64,
    /// Only meetings in this folder.
    pub folder_id: Option<String>,
    /// Only meetings carrying this tag, by `tags.name`.
    pub tag: Option<String>,
    /// Only meetings with this participant: a `people.id`, a display name or an
    /// email address, the last two case-insensitively.
    pub participant: Option<String>,
    /// Only meetings that started at or after this epoch-ms instant.
    pub started_after_ms: Option<i64>,
    /// Only meetings that started at or before this epoch-ms instant.
    pub started_before_ms: Option<i64>,
    /// Ranking weights; see [`SearchWeights`].
    pub weights: SearchWeights,
}

impl SearchQuery {
    /// A query for `text` with no filters and a page size of 50.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            limit: 50,
            offset: 0,
            folder_id: None,
            tag: None,
            participant: None,
            started_after_ms: None,
            started_before_ms: None,
            weights: SearchWeights::default(),
        }
    }

    /// Page size.
    #[must_use]
    pub fn limit(mut self, limit: i64) -> Self {
        self.limit = limit;
        self
    }

    /// Hits to skip.
    #[must_use]
    pub fn offset(mut self, offset: i64) -> Self {
        self.offset = offset;
        self
    }

    /// Restrict to one folder.
    #[must_use]
    pub fn folder(mut self, folder_id: impl Into<String>) -> Self {
        self.folder_id = Some(folder_id.into());
        self
    }

    /// Restrict to meetings carrying a tag, by name.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Restrict to meetings with a participant (`people.id`, display name or
    /// email).
    #[must_use]
    pub fn participant(mut self, participant: impl Into<String>) -> Self {
        self.participant = Some(participant.into());
        self
    }

    /// Restrict to a half-open-feeling but inclusive `started_at_ms` range.
    #[must_use]
    pub fn between(mut self, after_ms: i64, before_ms: i64) -> Self {
        self.started_after_ms = Some(after_ms);
        self.started_before_ms = Some(before_ms);
        self
    }

    /// Lower bound on `started_at_ms`, inclusive.
    #[must_use]
    pub fn after(mut self, ms: i64) -> Self {
        self.started_after_ms = Some(ms);
        self
    }

    /// Upper bound on `started_at_ms`, inclusive.
    #[must_use]
    pub fn before(mut self, ms: i64) -> Self {
        self.started_before_ms = Some(ms);
        self
    }

    /// Override the ranking weights.
    #[must_use]
    pub fn weights(mut self, weights: SearchWeights) -> Self {
        self.weights = weights;
        self
    }
}

/// One matching passage.
///
/// A meeting can appear several times — once per matching note, summary
/// version and transcript segment. That is deliberate: the unit a person wants
/// is "the place in the meeting where this was said", and collapsing to one row
/// per meeting throws away the timestamp that makes a hit playable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The meeting the hit belongs to.
    pub meeting_id: String,
    /// Its title, so a result list needs no second query.
    pub meeting_title: String,
    /// `meetings.started_at_ms`.
    pub started_at_ms: i64,
    /// Which index matched.
    pub source: SearchSource,
    /// Primary key of the matching row: a segment, note, summary or the meeting
    /// itself. UUIDv7 — never the FTS rowid, which §9.7 invariant 1 keeps
    /// internal.
    pub row_id: String,
    /// Milliseconds from meeting start, for a transcript hit. `None` for the
    /// other three sources, which are not positioned in time.
    pub start_ms: Option<i64>,
    /// `snippet()` output, with matched terms wrapped in `[` and `]`.
    pub snippet: String,
    /// Weighted `bm25()`. Negative; more negative is a better match.
    pub score: f64,
}

/// The `WHERE` fragment shared by all four branches of the ranking union.
///
/// Every filter is written `:param IS NULL OR <test>` so that one prepared
/// statement serves every combination of them. The alternative — concatenating
/// only the clauses that happen to be active — produces 32 different
/// statements, each planned separately, any of which could be the one with the
/// typo.
const FILTERS: &str = "
      AND (:folder IS NULL OR m.folder_id = :folder)
      AND (:after  IS NULL OR m.started_at_ms >= :after)
      AND (:before IS NULL OR m.started_at_ms <= :before)
      AND (:tag IS NULL OR EXISTS (
             SELECT 1 FROM meeting_tags mt
               JOIN tags t ON t.id = mt.tag_id
              WHERE mt.meeting_id = m.id AND t.name = :tag))
      AND (:participant IS NULL OR EXISTS (
             SELECT 1 FROM meeting_participants mp
              WHERE mp.meeting_id = m.id
                AND (mp.person_id = :participant
                     OR mp.display_name = :participant COLLATE NOCASE
                     OR mp.email = :participant COLLATE NOCASE)))";

impl Db {
    /// Search titles, notes, summaries and transcripts in one call.
    ///
    /// Ranked by `bm25()` scaled by [`SearchWeights`], previewed with
    /// `snippet()`, filtered by folder, tag, participant and date range.
    ///
    /// Returns [`StoreError::InvalidArgument`] when the input contains no
    /// searchable token at all — an empty `MATCH` is a syntax error in FTS5,
    /// and "everything" is not a defensible interpretation of "".
    ///
    /// # Why this is two statements and not one
    ///
    /// The obvious implementation ranks and renders in one union: `bm25()` and
    /// `snippet()` side by side, `ORDER BY score LIMIT 50`. It is also several
    /// times over §9.4's 100 ms budget, and the reason is instructive.
    /// `snippet()` on an *external-content* table cannot work from the index —
    /// it has to fetch the row from the source table and re-tokenize it. A
    /// two-term query over the acceptance corpus matches on the order of ten
    /// thousand segments, so the one-statement form renders ten thousand
    /// previews to show fifty. Measured on the 1,250-meeting /
    /// 150,000-segment corpus in `tests/search.rs`: **p95 371 ms**
    /// one-statement, **p95 18 ms** two-statement, against a 100 ms budget.
    ///
    /// So phase one ranks — index only, no source-table reads at all unless a
    /// filter needs them — and phase two renders a preview for the `LIMIT`
    /// rows that survived. Ranking is unchanged; only the number of previews
    /// is.
    pub fn search(&self, q: &SearchQuery) -> Result<Vec<SearchHit>> {
        let match_expr = to_match_expression(&q.text)?;
        let mut out = Vec::new();
        for (source, rowid, score) in self.rank(&match_expr, q)? {
            // A row can disappear between the two statements if another
            // connection deletes its meeting mid-search. Dropping it from the
            // page is right; failing the whole search is not.
            if let Some(hit) = self.hydrate(&match_expr, source, rowid, score)? {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// Phase one: which rows match, and in what order.
    fn rank(&self, match_expr: &str, q: &SearchQuery) -> Result<Vec<(SearchSource, i64, f64)>> {
        let filtered = q.folder_id.is_some()
            || q.tag.is_some()
            || q.participant.is_some()
            || q.started_after_ms.is_some()
            || q.started_before_ms.is_some();

        let conn = self.conn();
        let mut stmt = conn.prepare_cached(if filtered {
            &RANK_SQL_FILTERED
        } else {
            &RANK_SQL_PLAIN
        })?;

        let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
            (":q", &match_expr),
            (":w_title", &q.weights.title),
            (":w_note", &q.weights.note),
            (":w_summary", &q.weights.summary),
            (":w_transcript", &q.weights.transcript),
            (":limit", &q.limit),
            (":offset", &q.offset),
        ];
        if filtered {
            params.push((":folder", &q.folder_id));
            params.push((":tag", &q.tag));
            params.push((":participant", &q.participant));
            params.push((":after", &q.started_after_ms));
            params.push((":before", &q.started_before_ms));
        }

        let rows = stmt.query_map(&params[..], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (source, rowid, score) = row?;
            let source = SearchSource::from_str(&source).ok_or_else(|| {
                // Unreachable unless the union is edited without editing
                // `SearchSource`; loud beats a hit silently dropped.
                StoreError::InvalidArgument(format!("unknown search source `{source}`"))
            })?;
            out.push((source, rowid, score));
        }
        Ok(out)
    }

    /// Phase two: turn one ranked rowid into a displayable hit.
    fn hydrate(
        &self,
        match_expr: &str,
        source: SearchSource,
        rowid: i64,
        score: f64,
    ) -> Result<Option<SearchHit>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(hydrate_sql(source))?;
        let hit = stmt
            .query_row(
                named_params! {
                    ":q": match_expr,
                    ":rid": rowid,
                    ":open": SNIPPET_OPEN,
                    ":close": SNIPPET_CLOSE,
                    ":ellipsis": SNIPPET_ELLIPSIS,
                    ":snippet_tokens": SNIPPET_TOKENS,
                },
                |r| {
                    Ok(SearchHit {
                        meeting_id: r.get(0)?,
                        meeting_title: r.get(1)?,
                        started_at_ms: r.get(2)?,
                        source,
                        row_id: r.get(3)?,
                        start_ms: r.get(4)?,
                        snippet: r.get(5)?,
                        score,
                    })
                },
            )
            .optional()?;
        Ok(hit)
    }

    /// `search:rebuild` — reconstruct all four indexes from the source tables.
    ///
    /// §9.7 invariant 6: all derived state must be rebuildable by one command
    /// and is never synced. This is that command. It is also the recovery path
    /// for the two ways an external-content index can go wrong — a trigger that
    /// was missing when a row was written, and a full `VACUUM`, which may
    /// renumber the implicit rowids the index joins on.
    ///
    /// One transaction, so a crash halfway cannot leave three fresh indexes and
    /// one stale one.
    pub fn rebuild_search_index(&mut self) -> Result<()> {
        let tx = self.conn_mut().transaction()?;
        for table in FTS_TABLES {
            // `table` comes from a const array, never from a caller.
            tx.execute_batch(&format!("INSERT INTO {table}({table}) VALUES('rebuild');"))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Merge each index into a single b-tree, discarding retracted entries.
    ///
    /// A maintenance operation, not part of any hot path and deliberately not
    /// part of [`Db::delete_meeting`] — see the `optimize_all` docs in this
    /// module's source for what it costs and what makes it unnecessary there.
    pub fn optimize_search_index(&mut self) -> Result<()> {
        optimize_all(self.conn())
    }

    /// FTS5's own `integrity-check`, run against all four indexes.
    ///
    /// The `rank` argument is the whole point of this method and is not
    /// optional. Without it — `VALUES('integrity-check')` — FTS5 checks only
    /// that the index is internally well-formed, and an index that has silently
    /// lost every entry is perfectly well-formed. Passing 1 asks it to also
    /// verify that the index agrees with the *content table*, which is what
    /// turns this into a direct test for "a trigger did not fire", the one
    /// failure an external-content design is really exposed to. Verified by
    /// `the_integrity_check_notices_an_index_that_disagrees_with_its_content_table`,
    /// which fails against the argument-less form.
    ///
    /// Reports corruption as [`StoreError::Sqlite`] carrying SQLite's
    /// `database disk image is malformed`. The fix is
    /// [`Db::rebuild_search_index`], never a restore.
    pub fn verify_search_index(&self) -> Result<()> {
        for table in FTS_TABLES {
            self.conn().execute_batch(&format!(
                "INSERT INTO {table}({table}, rank) VALUES('integrity-check', 1);"
            ))?;
        }
        Ok(())
    }

    /// How many documents each index holds, as `(fts table, documents)`.
    ///
    /// Read straight out of the `%_docsize` shadow table, not out of the
    /// content table. That distinction is the whole point: after a meeting is
    /// deleted the content table is empty whatever happened, so counting it
    /// would prove nothing. `%_docsize` keeps one row per *indexed* document
    /// and is what still holds the tokens if a delete trigger fails to fire
    /// (§9.6).
    pub fn search_index_document_counts(&self) -> Result<Vec<(&'static str, i64)>> {
        let mut out = Vec::new();
        for table in FTS_TABLES {
            let n: i64 = self.conn().query_row(
                &format!("SELECT count(*) FROM {table}_docsize"),
                [],
                |r| r.get(0),
            )?;
            out.push((table, n));
        }
        Ok(out)
    }
}

/// Phase one with no filters: four index scans, `bm25()`, and nothing else.
///
/// Note what is *not* here — no join to `meetings`, `segments`, `notes` or
/// `summaries`. An unfiltered ranking never touches a source table, so its cost
/// is the doclist walk and nothing more.
static RANK_SQL_PLAIN: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    rank_sql(
        "FROM meetings_fts WHERE meetings_fts MATCH :q",
        "FROM notes_fts WHERE notes_fts MATCH :q",
        "FROM summaries_fts WHERE summaries_fts MATCH :q",
        "FROM segments_fts WHERE segments_fts MATCH :q",
    )
});

/// Phase one with filters: the same, plus the join to `meetings` that the
/// folder, tag, participant and date predicates need.
static RANK_SQL_FILTERED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    rank_sql(
        &format!(
            "FROM meetings_fts
               JOIN meetings m ON m.rowid = meetings_fts.rowid
              WHERE meetings_fts MATCH :q {FILTERS}"
        ),
        &format!(
            "FROM notes_fts
               JOIN notes n ON n.rowid = notes_fts.rowid
               JOIN meetings m ON m.id = n.meeting_id
              WHERE notes_fts MATCH :q {FILTERS}"
        ),
        &format!(
            "FROM summaries_fts
               JOIN summaries su ON su.rowid = summaries_fts.rowid
               JOIN meetings m ON m.id = su.meeting_id
              WHERE summaries_fts MATCH :q {FILTERS}"
        ),
        &format!(
            "FROM segments_fts
               JOIN segments sg ON sg.rowid = segments_fts.rowid
               JOIN meetings m ON m.id = sg.meeting_id
              WHERE segments_fts MATCH :q {FILTERS}"
        ),
    )
});

/// Assemble the four-branch ranking union from its per-source tails.
///
/// The `ORDER BY` tiebreak is `(source, rowid)` and not, say, "newest meeting
/// first": phase one deliberately does not have `started_at_ms` to hand in the
/// unfiltered case, and a tiebreak that changed depending on whether a filter
/// was set would make paging return the same hit twice.
fn rank_sql(meetings: &str, notes: &str, summaries: &str, segments: &str) -> String {
    format!(
        "WITH ranked(source, rid, score) AS (
             SELECT 'title', meetings_fts.rowid, bm25(meetings_fts, 1.0) * :w_title {meetings}
             UNION ALL
             SELECT 'note', notes_fts.rowid, bm25(notes_fts, 1.0) * :w_note {notes}
             UNION ALL
             SELECT 'summary', summaries_fts.rowid, bm25(summaries_fts, 1.0) * :w_summary {summaries}
             UNION ALL
             SELECT 'transcript', segments_fts.rowid, bm25(segments_fts, 1.0) * :w_transcript {segments}
         )
         SELECT source, rid, score FROM ranked
          ORDER BY score ASC, source ASC, rid ASC
          LIMIT :limit OFFSET :offset"
    )
}

/// Phase two: one ranked rowid in, one displayable hit out.
///
/// The `MATCH` is repeated here because `snippet()` needs to know *which*
/// phrase to highlight; the `rowid = :rid` beside it is what keeps this a seek
/// into the doclist rather than a second full scan.
fn hydrate_sql(source: SearchSource) -> &'static str {
    match source {
        SearchSource::Title => {
            "SELECT m.id, m.title, m.started_at_ms, m.id, NULL,
                    snippet(meetings_fts, 0, :open, :close, :ellipsis, :snippet_tokens)
               FROM meetings_fts
               JOIN meetings m ON m.rowid = meetings_fts.rowid
              WHERE meetings_fts MATCH :q AND meetings_fts.rowid = :rid"
        }
        SearchSource::Note => {
            "SELECT m.id, m.title, m.started_at_ms, n.id, NULL,
                    snippet(notes_fts, 0, :open, :close, :ellipsis, :snippet_tokens)
               FROM notes_fts
               JOIN notes n ON n.rowid = notes_fts.rowid
               JOIN meetings m ON m.id = n.meeting_id
              WHERE notes_fts MATCH :q AND notes_fts.rowid = :rid"
        }
        SearchSource::Summary => {
            "SELECT m.id, m.title, m.started_at_ms, su.id, NULL,
                    snippet(summaries_fts, 0, :open, :close, :ellipsis, :snippet_tokens)
               FROM summaries_fts
               JOIN summaries su ON su.rowid = summaries_fts.rowid
               JOIN meetings m ON m.id = su.meeting_id
              WHERE summaries_fts MATCH :q AND summaries_fts.rowid = :rid"
        }
        SearchSource::Transcript => {
            "SELECT m.id, m.title, m.started_at_ms, sg.id, sg.start_ms,
                    snippet(segments_fts, 0, :open, :close, :ellipsis, :snippet_tokens)
               FROM segments_fts
               JOIN segments sg ON sg.rowid = segments_fts.rowid
               JOIN meetings m ON m.id = sg.meeting_id
              WHERE segments_fts MATCH :q AND segments_fts.rowid = :rid"
        }
    }
}

/// Merge every b-tree in every FTS index into one, discarding retracted
/// entries and reclaiming the space they held.
///
/// # Why this is not what `delete_meeting` uses
///
/// By default a `'delete'` against an FTS5 index is *logical*. It appends
/// delete markers, and a delete marker carries the term it is retracting — so
/// retracting a transcript does not remove its words from the file, it writes a
/// **second copy of every one of them**. Queries stop returning the row
/// immediately, which is what makes this so easy to miss: every behavioural
/// test passes while the text is still sitting in `segments_fts_data` in plain
/// form, and only §9.6's byte scan sees it.
///
/// `optimize` does collapse the markers against the postings they cancel, and
/// it was how [`Db::delete_meeting`] first satisfied §9.6. It is not how it
/// does now, because it rewrites the *whole* index: deleting one meeting from a
/// 20,000-meeting library would cost time proportional to the library. Setting
/// FTS5's `'secure-delete'` in migration 0002 instead makes the retraction
/// itself rewrite only the affected leaf pages, which is proportional to the
/// deletion, and leaves nothing for this to collapse.
///
/// What remains here is a maintenance operation: an index that has absorbed a
/// year of edited notes and regenerated summaries is spread across many
/// b-trees, and merging them speeds up queries. Nothing on the recording path
/// may call it.
pub(crate) fn optimize_all(conn: &rusqlite::Connection) -> Result<()> {
    for table in FTS_TABLES {
        conn.execute_batch(&format!("INSERT INTO {table}({table}) VALUES('optimize');"))?;
    }
    Ok(())
}

/// Turn arbitrary user input into a legal FTS5 `MATCH` expression.
///
/// Splits on everything `unicode61` treats as a separator, quotes each token as
/// a one-word phrase, and ANDs them. Quoting is what makes the result total:
/// `"and"` is a string, `AND` is an operator, and a user who types `AND` means
/// the word.
///
/// Deliberately drops FTS5's operator syntax rather than passing it through.
/// `NEAR`, `^`, `*` and column filters are useful and none of them survive
/// contact with a search box that also has to accept `C++`, `re: Q3` and a
/// pasted email address.
fn to_match_expression(input: &str) -> Result<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }

    if terms.is_empty() {
        return Err(StoreError::InvalidArgument(format!(
            "search query `{input}` contains no searchable word"
        )));
    }

    let quoted: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    Ok(quoted.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_term_query_becomes_two_quoted_phrases_anded() {
        assert_eq!(
            to_match_expression("quarterly revenue").unwrap(),
            "\"quarterly\" AND \"revenue\""
        );
    }

    /// The inputs that would otherwise be FTS5 syntax errors or, worse,
    /// accidental operators.
    #[test]
    fn operator_syntax_in_user_input_is_neutralised() {
        for (input, expected) in [
            ("C++", "\"C\""),
            ("a AND b", "\"a\" AND \"AND\" AND \"b\""),
            ("\"unbalanced", "\"unbalanced\""),
            ("NEAR(a b)", "\"NEAR\" AND \"a\" AND \"b\""),
            ("re: Q3", "\"re\" AND \"Q3\""),
            ("ada@example.com", "\"ada\" AND \"example\" AND \"com\""),
            ("*", ""),
        ] {
            match to_match_expression(input) {
                Ok(got) => assert_eq!(got, expected, "for input {input:?}"),
                Err(e) => assert!(expected.is_empty(), "for input {input:?}: {e}"),
            }
        }
    }

    /// Diacritics survive tokenisation here; folding them is the tokenizer's
    /// job, and doing it twice would break `remove_diacritics 2`'s handling of
    /// composed sequences.
    #[test]
    fn non_ascii_words_are_kept_whole() {
        assert_eq!(
            to_match_expression("Zürich café").unwrap(),
            "\"Zürich\" AND \"café\""
        );
    }

    #[test]
    fn a_query_with_no_word_in_it_is_refused() {
        let err = to_match_expression("   ...  ").unwrap_err();
        assert!(matches!(err, StoreError::InvalidArgument(_)), "got {err:?}");
    }

    /// The default weights are load-bearing for §9.4's "titles and notes above
    /// transcript body", so they are pinned rather than left to drift.
    #[test]
    fn default_weights_put_titles_and_notes_above_transcript_body() {
        let w = SearchWeights::default();
        assert!(w.title > w.note);
        assert!(w.note > w.summary);
        assert!(w.summary > w.transcript);
        assert_eq!(
            w.transcript, 1.0,
            "transcript body is the unit of the scale"
        );
    }
}

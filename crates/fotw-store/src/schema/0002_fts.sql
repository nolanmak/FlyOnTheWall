-- Migration 0002 -- full-text search (docs/REQUIREMENTS.md 9.4).
--
-- Four FTS5 **external-content** tables, one per thing a person actually
-- searches for: a meeting title, their own notes, a generated summary, and the
-- transcript body. External content (`content='<table>'`) means the tokens live
-- in the index and the *columns* are read back from the source table through
-- the shared rowid, so the text is stored once rather than twice. A 250 MB/year
-- transcript corpus (§9.5) would otherwise be 500 MB.
--
-- Three consequences of that choice, each of which this file has to handle:
--
--   1. **The index does not update itself.** Nothing in SQLite connects
--      `INSERT INTO segments` to `segments_fts`; the AFTER INSERT/UPDATE/DELETE
--      triggers below are the entire synchronisation mechanism. A missing
--      trigger is not a compile error, it is a search result that silently
--      never appears.
--
--   2. **Deleting the source row does NOT remove the tokens.** This is the
--      subtle half of §9.6: `DELETE FROM segments` leaves every word of that
--      transcript sitting in `segments_fts_data`. The delete trigger uses the
--      documented `INSERT INTO ft(ft, rowid, ...) VALUES('delete', ...)` form,
--      which is the only way to retract a row from an external-content index —
--      and it must be passed the *old* column values, because FTS5 recomputes
--      the tokens from them to know which postings to remove. Passing anything
--      else corrupts the index instead of shrinking it.
--
--      And retracting is still not enough on its own, which is the part that
--      only a byte scan catches. By default an FTS5 retraction is *logical*: it
--      appends a delete marker, and a delete marker carries the term it is
--      retracting, so deleting a transcript writes a SECOND copy of every one
--      of its words into the file. Queries go quiet immediately, so every
--      behavioural test passes while the text is still there in plain form.
--      `'secure-delete'` (below) is the option that makes FTS5 rewrite the
--      affected leaf pages instead; `PRAGMA secure_delete = ON` from §9.1 then
--      zeroes the pages it frees, and `PRAGMA incremental_vacuum` returns them.
--      All three are needed and none of them is observable from a query.
--
--   3. **Rowids are the join key, so rowid stability is load-bearing.**
--      §9.7 invariant 1 already reserves implicit rowids for exactly this and
--      forbids exporting them. The hazard to remember is that a full `VACUUM`
--      may renumber the rowids of a table with no INTEGER PRIMARY KEY, which
--      would silently point every index entry at the wrong row. `delete_meeting`
--      uses `PRAGMA incremental_vacuum`, which does not renumber; if a full
--      VACUUM is ever added it MUST be followed by `search:rebuild`.
--
-- TOKENIZER. `unicode61 remove_diacritics 2` everywhere, and deliberately NOT
-- `porter`. Stemming is a good trade for prose and a bad one here: what people
-- search meetings for is product names, acronyms and people, and Porter maps
-- "Universal" and "universe" onto the same stem `univers`, "Datadog" and
-- "datadogs" onto `datadog`, "operator" and "operating" onto `oper`. Recall
-- bought that way costs precision on exactly the queries that matter.
-- `remove_diacritics 2` is the fixed version of the option (1 misses codepoints
-- that carry more than one diacritic), so "Zurich" finds "Zürich".
--
-- WEIGHTS. §9.4 asks for titles and notes to outrank transcript body. See
-- `SearchWeights` in src/search.rs for where that lives and why it cannot be
-- expressed as a bm25() column weight alone.

------------------------------------------------------------------ the indexes
--
-- `'secure-delete'` is set immediately after each CREATE, before any row is
-- indexed, because it is a persistent `%_config` setting that only governs
-- deletes issued while it is on. Setting it late would leave every row written
-- before it as a row whose deletion is still only logical.

CREATE VIRTUAL TABLE meetings_fts USING fts5(
  title,
  content = 'meetings',
  content_rowid = 'rowid',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO meetings_fts(meetings_fts, rank) VALUES('secure-delete', 1);

CREATE VIRTUAL TABLE notes_fts USING fts5(
  body_md,
  content = 'notes',
  content_rowid = 'rowid',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO notes_fts(notes_fts, rank) VALUES('secure-delete', 1);

CREATE VIRTUAL TABLE summaries_fts USING fts5(
  body_md,
  content = 'summaries',
  content_rowid = 'rowid',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO summaries_fts(summaries_fts, rank) VALUES('secure-delete', 1);

CREATE VIRTUAL TABLE segments_fts USING fts5(
  text,
  content = 'segments',
  content_rowid = 'rowid',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO segments_fts(segments_fts, rank) VALUES('secure-delete', 1);

----------------------------------------------------------------- the triggers

-- meetings.title
--
-- `_au` fires on an UPDATE of ANY column, not only `title`. Since #74 that
-- means every `set_enrich_report` -- three an hour on a live library -- pays a
-- delete-and-reinsert of an unchanged title. Known, measured and deliberately
-- left (#89): narrowing it to `AFTER UPDATE OF title` cannot be done in place,
-- because a released migration is immutable (see `migrations.rs`), and doing it
-- as a 0004 bumps LATEST_SCHEMA_VERSION, at which point every shipped binary
-- refuses the library. That is a real behaviour change bought for three
-- redundant index writes an hour. Note also that the scoped form fires on
-- `title` appearing in the SET list rather than on its value changing, and
-- would stop firing on a statement that moved a rowid without naming `title`;
-- a trigger that under-fires on an external-content index corrupts search
-- silently, which is why this one is broad on purpose.
CREATE TRIGGER meetings_fts_ai AFTER INSERT ON meetings BEGIN
  INSERT INTO meetings_fts(rowid, title) VALUES (new.rowid, new.title);
END;
CREATE TRIGGER meetings_fts_ad AFTER DELETE ON meetings BEGIN
  INSERT INTO meetings_fts(meetings_fts, rowid, title) VALUES ('delete', old.rowid, old.title);
END;
CREATE TRIGGER meetings_fts_au AFTER UPDATE ON meetings BEGIN
  INSERT INTO meetings_fts(meetings_fts, rowid, title) VALUES ('delete', old.rowid, old.title);
  INSERT INTO meetings_fts(rowid, title) VALUES (new.rowid, new.title);
END;

-- notes.body_md
CREATE TRIGGER notes_fts_ai AFTER INSERT ON notes BEGIN
  INSERT INTO notes_fts(rowid, body_md) VALUES (new.rowid, new.body_md);
END;
CREATE TRIGGER notes_fts_ad AFTER DELETE ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, body_md) VALUES ('delete', old.rowid, old.body_md);
END;
CREATE TRIGGER notes_fts_au AFTER UPDATE ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, body_md) VALUES ('delete', old.rowid, old.body_md);
  INSERT INTO notes_fts(rowid, body_md) VALUES (new.rowid, new.body_md);
END;

-- summaries.body_md. Every version is indexed, not just is_current: the
-- append-only history is the point of the table (§9.3), and a search that could
-- not reach a superseded summary would quietly make regeneration destructive
-- from the user's point of view.
CREATE TRIGGER summaries_fts_ai AFTER INSERT ON summaries BEGIN
  INSERT INTO summaries_fts(rowid, body_md) VALUES (new.rowid, new.body_md);
END;
CREATE TRIGGER summaries_fts_ad AFTER DELETE ON summaries BEGIN
  INSERT INTO summaries_fts(summaries_fts, rowid, body_md) VALUES ('delete', old.rowid, old.body_md);
END;
CREATE TRIGGER summaries_fts_au AFTER UPDATE ON summaries BEGIN
  INSERT INTO summaries_fts(summaries_fts, rowid, body_md) VALUES ('delete', old.rowid, old.body_md);
  INSERT INTO summaries_fts(rowid, body_md) VALUES (new.rowid, new.body_md);
END;

-- segments.text -- the big one, and the one §9.6 is about.
CREATE TRIGGER segments_fts_ai AFTER INSERT ON segments BEGIN
  INSERT INTO segments_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER segments_fts_ad AFTER DELETE ON segments BEGIN
  INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER segments_fts_au AFTER UPDATE ON segments BEGIN
  INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
  INSERT INTO segments_fts(rowid, text) VALUES (new.rowid, new.text);
END;

--------------------------------------------------------------- the first fill
--
-- 0001 has shipped, so this migration runs against libraries that already hold
-- meetings. The triggers above only see rows written from now on; these four
-- statements are what indexes everything that is already there. They are the
-- same 'rebuild' command `search:rebuild` issues, which is §9.7 invariant 6
-- ("all derived state rebuildable by one command") being used rather than
-- merely claimed: if this file needed a bespoke backfill query, the invariant
-- would already be broken.

INSERT INTO meetings_fts(meetings_fts) VALUES('rebuild');
INSERT INTO notes_fts(notes_fts) VALUES('rebuild');
INSERT INTO summaries_fts(summaries_fts) VALUES('rebuild');
INSERT INTO segments_fts(segments_fts) VALUES('rebuild');

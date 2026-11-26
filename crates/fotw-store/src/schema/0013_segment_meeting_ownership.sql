-- Migration 0013 -- a transcript's rows belong to the transcript's meeting (#343).
--
-- Until now a segment could name meeting B and meeting A's transcript: the
-- schema had one reference to transcripts(id) and another to meetings(id), and
-- nothing tied them together. #336 showed what that costs (B's words in A's
-- summary); #338 scoped the readers and the importer. This file makes the rule
-- a database invariant: transcripts(id, meeting_id) becomes a parent key, and
-- every table that carries both a transcript id and a meeting id references
-- the pair.
--
-- RUNS WITH foreign_keys OFF. SQLite cannot add a constraint to an existing
-- table, so the five child tables are rebuilt the documented way
-- (https://sqlite.org/lang_altertable.html#otheralter): create <t>_new, copy,
-- drop <t>, rename <t>_new to <t>. With foreign_keys ON, that DROP would run an
-- implicit DELETE whose ON DELETE CASCADE wipes action_items (child of
-- summaries), post_pass_windows (child of post_pass_jobs) and speech_receipts
-- (child of speech_epochs). `migrations::migrate` switches enforcement off
-- around the migration run, and the hook on this migration runs
-- `PRAGMA foreign_key_check` on every rebuilt table before the transaction
-- commits, so nothing that violates a reference can land.
--
-- The rename direction matters: <t>_new is renamed to <t>, never <t> to
-- <t>_old. Since SQLite 3.26 a rename rewrites the REFERENCES clauses of other
-- tables, so renaming the old table would leave action_items pointing at a
-- dropped summaries_old.
--
-- ROWIDS ARE COPIED. segments and summaries back external-content FTS5 indexes
-- keyed by rowid (0002). Copying `rowid` explicitly keeps every index entry
-- pointing at the same row; letting SQLite renumber would silently aim search
-- hits at the wrong segment.
--
-- TRIGGERS AND INDEXES ON A DROPPED TABLE GO WITH IT. Each one is recreated
-- below verbatim from 0001, 0002, 0007, 0010 and 0011, and
-- `migrations::tests` checks that the set of triggers, indexes and columns is
-- the same before and after this migration.

------------------------------------------------------------------ parent key

-- A UNIQUE index is a valid foreign-key parent key, so transcripts itself does
-- not have to be rebuilt (and nothing referencing it has to be touched).
CREATE UNIQUE INDEX transcripts_id_meeting_uidx ON transcripts(id, meeting_id);

---------------------------------------------------------------------- repair
--
-- Rows that already disagree (a library that imported an edited archive
-- before #338) are found first, while the old tables' triggers are still
-- attached: a removed segment's words are retracted from segments_fts and each
-- touched meeting gets a new summary-input generation, exactly as an ordinary
-- DELETE or UPDATE would.
--
-- What happens to each kind of row:
--
--   segments, speech_epochs: RE-HOMED to the transcript's meeting only when
--     that is the one consistent repair -- the meeting the row names has no
--     transcript at all, so the row's transcript is the only home it can have.
--     Otherwise the row is REMOVED: when the named meeting has transcripts of
--     its own, either id could be the wrong one, and guessing would either move
--     one meeting's words into another meeting's transcript (the #336 leak) or
--     invent a transcript membership. Since #338 these rows were invisible to
--     every transcript reader anyway. A row whose transcript no longer exists
--     is removed too.
--   speech_receipts: follow their epoch (the existing 0007 reference ties the
--     two). A receipt whose epoch is removed is removed.
--   summaries.transcript_id, post_pass_jobs.{live,cleaned}_transcript_id:
--     CLEARED to NULL, the same thing ON DELETE SET NULL does. The summary or
--     job belongs to its meeting; only the pointer was wrong. Counted per
--     pointer, so a job with both ids wrong counts twice.
--
-- The counts are written to app_meta as `ownership_repair:<table>` =
-- {"found":n,"rehomed":n,"removed":n,"cleared":n}, only for tables where
-- something was found. `Db::ownership_repairs` reads them and the daemon
-- journals them at startup.

CREATE TEMP TABLE ownership_repair (
  tbl TEXT NOT NULL,
  row_key TEXT NOT NULL,
  col TEXT NOT NULL DEFAULT '',
  action TEXT NOT NULL CHECK(action IN ('rehome', 'remove', 'clear')),
  home TEXT,
  PRIMARY KEY (tbl, row_key, col)
);

INSERT INTO temp.ownership_repair (tbl, row_key, action, home)
  SELECT 'segments', s.id,
         CASE WHEN t.id IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM transcripts o WHERE o.meeting_id = s.meeting_id)
              THEN 'rehome' ELSE 'remove' END,
         t.meeting_id
    FROM segments s LEFT JOIN transcripts t ON t.id = s.transcript_id
   WHERE t.id IS NULL OR t.meeting_id <> s.meeting_id;

INSERT INTO temp.ownership_repair (tbl, row_key, action, home)
  SELECT 'speech_epochs', e.id,
         CASE WHEN t.id IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM transcripts o WHERE o.meeting_id = e.meeting_id)
              THEN 'rehome' ELSE 'remove' END,
         t.meeting_id
    FROM speech_epochs e LEFT JOIN transcripts t ON t.id = e.transcript_id
   WHERE t.id IS NULL OR t.meeting_id <> e.meeting_id;

INSERT INTO temp.ownership_repair (tbl, row_key, action, home)
  SELECT 'speech_receipts', r.id,
         CASE WHEN t.id IS NOT NULL
                   AND EXISTS (SELECT 1 FROM temp.ownership_repair o
                                WHERE o.tbl = 'speech_epochs' AND o.row_key = r.epoch_id
                                  AND o.action = 'rehome' AND o.home = t.meeting_id)
              THEN 'rehome' ELSE 'remove' END,
         t.meeting_id
    FROM speech_receipts r LEFT JOIN transcripts t ON t.id = r.transcript_id
   WHERE t.id IS NULL OR t.meeting_id <> r.meeting_id
      OR r.epoch_id IN (SELECT row_key FROM temp.ownership_repair
                         WHERE tbl = 'speech_epochs' AND action = 'remove');

INSERT INTO temp.ownership_repair (tbl, row_key, action)
  SELECT 'summaries', s.id, 'clear'
    FROM summaries s LEFT JOIN transcripts t ON t.id = s.transcript_id
   WHERE s.transcript_id IS NOT NULL AND (t.id IS NULL OR t.meeting_id <> s.meeting_id);

INSERT INTO temp.ownership_repair (tbl, row_key, col, action)
  SELECT 'post_pass_jobs', j.meeting_id, 'live', 'clear'
    FROM post_pass_jobs j LEFT JOIN transcripts t ON t.id = j.live_transcript_id
   WHERE j.live_transcript_id IS NOT NULL AND (t.id IS NULL OR t.meeting_id <> j.meeting_id);

INSERT INTO temp.ownership_repair (tbl, row_key, col, action)
  SELECT 'post_pass_jobs', j.meeting_id, 'cleaned', 'clear'
    FROM post_pass_jobs j LEFT JOIN transcripts t ON t.id = j.cleaned_transcript_id
   WHERE j.cleaned_transcript_id IS NOT NULL AND (t.id IS NULL OR t.meeting_id <> j.meeting_id);

INSERT INTO app_meta (key, value, updated_at)
  SELECT 'ownership_repair:' || tbl,
         json_object('found', count(*),
                     'rehomed', sum(action = 'rehome'),
                     'removed', sum(action = 'remove'),
                     'cleared', sum(action = 'clear')),
         CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
    FROM temp.ownership_repair
   GROUP BY tbl;

-- Receipts before epochs, epochs before nothing: with enforcement off the
-- order only has to leave a consistent end state, but this one never passes
-- through a state a reader could not explain.
UPDATE speech_receipts
   SET meeting_id = (SELECT home FROM temp.ownership_repair o
                      WHERE o.tbl = 'speech_receipts' AND o.row_key = speech_receipts.id)
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'speech_receipts' AND action = 'rehome');
DELETE FROM speech_receipts
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'speech_receipts' AND action = 'remove');
UPDATE speech_epochs
   SET meeting_id = (SELECT home FROM temp.ownership_repair o
                      WHERE o.tbl = 'speech_epochs' AND o.row_key = speech_epochs.id)
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'speech_epochs' AND action = 'rehome');
DELETE FROM speech_receipts
 WHERE epoch_id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'speech_epochs' AND action = 'remove');
DELETE FROM speech_epochs
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'speech_epochs' AND action = 'remove');

UPDATE segments
   SET meeting_id = (SELECT home FROM temp.ownership_repair o
                      WHERE o.tbl = 'segments' AND o.row_key = segments.id)
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'segments' AND action = 'rehome');
DELETE FROM segments
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'segments' AND action = 'remove');

UPDATE summaries SET transcript_id = NULL
 WHERE id IN (SELECT row_key FROM temp.ownership_repair WHERE tbl = 'summaries');

UPDATE post_pass_jobs SET live_transcript_id = NULL
 WHERE meeting_id IN (SELECT row_key FROM temp.ownership_repair
                       WHERE tbl = 'post_pass_jobs' AND col = 'live');
UPDATE post_pass_jobs SET cleaned_transcript_id = NULL
 WHERE meeting_id IN (SELECT row_key FROM temp.ownership_repair
                       WHERE tbl = 'post_pass_jobs' AND col = 'cleaned');

DROP TABLE temp.ownership_repair;

-------------------------------------------------------------------- segments
--
-- The single-column references stay: they keep the exact ON DELETE behaviour
-- every caller relies on. The combined reference is what is new.

CREATE TABLE segments_new (
  id TEXT PRIMARY KEY,
  transcript_id TEXT NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL,
  start_ms INTEGER NOT NULL,
  end_ms INTEGER NOT NULL,
  channel TEXT NOT NULL,                         -- mic|system
  speaker_label TEXT,
  person_id TEXT REFERENCES people(id) ON DELETE SET NULL,
  text TEXT NOT NULL,
  confidence REAL,
  is_final INTEGER NOT NULL DEFAULT 1,
  words BLOB,                                    -- zstd(JSON [{w,s,e,c,sp}])
  UNIQUE (transcript_id, idx),
  FOREIGN KEY (transcript_id, meeting_id)
    REFERENCES transcripts(id, meeting_id) ON DELETE CASCADE
) STRICT;
INSERT INTO segments_new (rowid, id, transcript_id, meeting_id, idx, start_ms, end_ms, channel,
                          speaker_label, person_id, text, confidence, is_final, words)
  SELECT rowid, id, transcript_id, meeting_id, idx, start_ms, end_ms, channel,
         speaker_label, person_id, text, confidence, is_final, words
    FROM segments;
DROP TABLE segments;
ALTER TABLE segments_new RENAME TO segments;
CREATE INDEX segments_meeting_time_idx ON segments(meeting_id, start_ms);

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

CREATE TRIGGER summary_input_segments_insert AFTER INSERT ON segments BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_segments_update AFTER UPDATE ON segments BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_segments_delete AFTER DELETE ON segments BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

---------------------------------------------------------------- speech epochs

CREATE TABLE speech_epochs_new (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  transcript_id TEXT NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
  session_id TEXT NOT NULL,
  source TEXT NOT NULL CHECK(source IN ('system','mic')),
  epoch INTEGER NOT NULL CHECK(epoch >= 0),
  from_sample INTEGER NOT NULL CHECK(from_sample >= 0),
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  revision TEXT,
  capabilities_json TEXT NOT NULL CHECK(json_valid(capabilities_json)),
  reason TEXT,
  UNIQUE(session_id, source, epoch),
  UNIQUE(id, meeting_id, transcript_id),
  FOREIGN KEY(transcript_id, meeting_id)
    REFERENCES transcripts(id, meeting_id) ON DELETE CASCADE
) STRICT;
INSERT INTO speech_epochs_new (rowid, id, meeting_id, transcript_id, session_id, source, epoch,
                               from_sample, provider, model, revision, capabilities_json, reason)
  SELECT rowid, id, meeting_id, transcript_id, session_id, source, epoch,
         from_sample, provider, model, revision, capabilities_json, reason
    FROM speech_epochs;
DROP TABLE speech_epochs;
ALTER TABLE speech_epochs_new RENAME TO speech_epochs;
CREATE INDEX speech_epochs_meeting_idx ON speech_epochs(meeting_id);

CREATE TRIGGER summary_input_speech_epochs_insert AFTER INSERT ON speech_epochs BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_epochs_update AFTER UPDATE ON speech_epochs BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_epochs_delete AFTER DELETE ON speech_epochs BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

-------------------------------------------------------------- speech receipts

CREATE TABLE speech_receipts_new (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  transcript_id TEXT NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
  epoch_id TEXT NOT NULL,
  from_sample INTEGER NOT NULL CHECK(from_sample >= 0),
  through_sample INTEGER NOT NULL CHECK(through_sample > from_sample),
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  FOREIGN KEY(epoch_id, meeting_id, transcript_id)
    REFERENCES speech_epochs(id, meeting_id, transcript_id) ON DELETE CASCADE,
  FOREIGN KEY(transcript_id, meeting_id)
    REFERENCES transcripts(id, meeting_id) ON DELETE CASCADE,
  UNIQUE(epoch_id, from_sample)
) STRICT;
INSERT INTO speech_receipts_new (rowid, id, meeting_id, transcript_id, epoch_id, from_sample,
                                 through_sample, payload_json)
  SELECT rowid, id, meeting_id, transcript_id, epoch_id, from_sample, through_sample, payload_json
    FROM speech_receipts;
DROP TABLE speech_receipts;
ALTER TABLE speech_receipts_new RENAME TO speech_receipts;
CREATE INDEX speech_receipts_meeting_idx ON speech_receipts(meeting_id);

CREATE TRIGGER summary_input_speech_receipts_insert AFTER INSERT ON speech_receipts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_receipts_update AFTER UPDATE ON speech_receipts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_receipts_delete AFTER DELETE ON speech_receipts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

-------------------------------------------------------------------- summaries
--
-- The combined reference has no ON DELETE action of its own. SQLite's SET NULL
-- nulls every column of a reference, and meeting_id is NOT NULL, so the
-- single-column `ON DELETE SET NULL` above it is what clears transcript_id when
-- the transcript goes; a reference with a NULL column is satisfied, so the
-- combined one then holds. Same for post_pass_jobs below.

CREATE TABLE summaries_new (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  template_id TEXT REFERENCES templates(id) ON DELETE SET NULL,
  transcript_id TEXT REFERENCES transcripts(id) ON DELETE SET NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  prompt_hash TEXT NOT NULL,
  body_md TEXT NOT NULL,
  coverage REAL,                                 -- §8.6 citation-coverage metric
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_micros INTEGER,
  is_current INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  origin_device_id TEXT NOT NULL,
  UNIQUE (meeting_id, version),
  FOREIGN KEY (transcript_id, meeting_id) REFERENCES transcripts(id, meeting_id)
) STRICT;
INSERT INTO summaries_new (rowid, id, meeting_id, version, template_id, transcript_id, provider,
                           model, prompt_hash, body_md, coverage, input_tokens, output_tokens,
                           cost_micros, is_current, created_at, origin_device_id)
  SELECT rowid, id, meeting_id, version, template_id, transcript_id, provider,
         model, prompt_hash, body_md, coverage, input_tokens, output_tokens,
         cost_micros, is_current, created_at, origin_device_id
    FROM summaries;
DROP TABLE summaries;
ALTER TABLE summaries_new RENAME TO summaries;
CREATE UNIQUE INDEX summaries_current_uidx ON summaries(meeting_id) WHERE is_current = 1;

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

--------------------------------------------------------------- post-pass jobs

CREATE TABLE post_pass_jobs_new (
  meeting_id TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
  state TEXT NOT NULL CHECK(state IN ('pending', 'done', 'failed')),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  plan_json TEXT NOT NULL CHECK(json_valid(plan_json)),
  progress_permille INTEGER NOT NULL DEFAULT 0
    CHECK(progress_permille BETWEEN 0 AND 1000),
  live_transcript_id TEXT REFERENCES transcripts(id) ON DELETE SET NULL,
  cleaned_transcript_id TEXT REFERENCES transcripts(id) ON DELETE SET NULL,
  provenance_json TEXT CHECK(provenance_json IS NULL OR json_valid(provenance_json)),
  detail TEXT,
  FOREIGN KEY (live_transcript_id, meeting_id) REFERENCES transcripts(id, meeting_id),
  FOREIGN KEY (cleaned_transcript_id, meeting_id) REFERENCES transcripts(id, meeting_id)
) STRICT;
INSERT INTO post_pass_jobs_new (rowid, meeting_id, state, created_at_ms, updated_at_ms, plan_json,
                                progress_permille, live_transcript_id, cleaned_transcript_id,
                                provenance_json, detail)
  SELECT rowid, meeting_id, state, created_at_ms, updated_at_ms, plan_json,
         progress_permille, live_transcript_id, cleaned_transcript_id,
         provenance_json, detail
    FROM post_pass_jobs;
DROP TABLE post_pass_jobs;
ALTER TABLE post_pass_jobs_new RENAME TO post_pass_jobs;
CREATE INDEX post_pass_jobs_state_idx ON post_pass_jobs(state);

-- A summary records the slug of the template file used to generate it.
-- The file can be edited, renamed or deleted independently of the library.
-- Keep existing values and preserve rowids for the external-content FTS index.
-- Foreign keys are disabled during the rebuild to preserve child action items.

CREATE TABLE summaries_new (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  template_id TEXT,
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

-- Migration 0014 -- show which calendar event a release was matched to, and
-- let a person reject a wrong match (#326 follow-up).
--
-- The matched event's scheduled start and end, frozen with the title when the
-- row is created. Rows created before this migration keep NULL: the event is
-- not re-read, so they show the title only.
ALTER TABLE meeting_releases ADD COLUMN event_start_ms INTEGER;
ALTER TABLE meeting_releases ADD COLUMN event_end_ms INTEGER;

-- A new decision, `wrong_event`: the person said the match was wrong. It is
-- recorded for the log and never counts toward the auto unlock (D11, D19).
--
-- SQLite cannot change a CHECK constraint in place, so release_decisions is
-- rebuilt the documented way (create _new, copy, drop, rename _new). No table
-- references it and it references none, so the rebuild touches nothing else.
-- Its index and append-only trigger go with the dropped table and are
-- recreated verbatim from 0012.
CREATE TABLE release_decisions_new (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL,
  engine_id TEXT NOT NULL,
  decision TEXT NOT NULL CHECK(decision IN
    ('approved_unchanged', 'approved_edited', 'dropped', 'auto', 'sent_again', 'wrong_event')),
  at_ms INTEGER NOT NULL
) STRICT;
INSERT INTO release_decisions_new (rowid, id, meeting_id, engine_id, decision, at_ms)
  SELECT rowid, id, meeting_id, engine_id, decision, at_ms FROM release_decisions;
DROP TABLE release_decisions;
ALTER TABLE release_decisions_new RENAME TO release_decisions;
CREATE INDEX release_decisions_engine_idx ON release_decisions(engine_id, decision);
CREATE TRIGGER release_decisions_append_only BEFORE UPDATE ON release_decisions BEGIN
  SELECT RAISE(ABORT, 'release_decisions is append-only');
END;

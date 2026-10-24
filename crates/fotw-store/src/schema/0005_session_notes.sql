-- Drafts remain encrypted with the library, before any meeting row exists.
CREATE TABLE session_notes (
  session_id TEXT PRIMARY KEY NOT NULL,
  started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
  revision INTEGER NOT NULL CHECK(revision > 0),
  body_md TEXT NOT NULL,
  anchors_json TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
) STRICT;

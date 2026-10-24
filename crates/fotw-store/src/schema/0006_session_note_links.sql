-- A permanent alias lets a late session save reach the canonical meeting note.
CREATE TABLE session_note_links (
  session_id TEXT PRIMARY KEY NOT NULL,
  meeting_id TEXT NOT NULL UNIQUE REFERENCES meetings(id) ON DELETE CASCADE,
  started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
  elapsed_ms INTEGER NOT NULL CHECK(elapsed_ms >= 0)
) STRICT;

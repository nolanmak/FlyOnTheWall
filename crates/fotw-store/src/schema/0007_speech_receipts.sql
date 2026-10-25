-- Immutable source/provider epochs and original inference receipts. User edits
-- live in segments; replay compares receipts without replacing those edits.
CREATE TABLE speech_epochs (
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
  UNIQUE(id, meeting_id, transcript_id)
) STRICT;
CREATE INDEX speech_epochs_meeting_idx ON speech_epochs(meeting_id);
CREATE TABLE speech_receipts (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  transcript_id TEXT NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
  epoch_id TEXT NOT NULL,
  from_sample INTEGER NOT NULL CHECK(from_sample >= 0),
  through_sample INTEGER NOT NULL CHECK(through_sample > from_sample),
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  FOREIGN KEY(epoch_id, meeting_id, transcript_id)
    REFERENCES speech_epochs(id, meeting_id, transcript_id) ON DELETE CASCADE,
  UNIQUE(epoch_id, from_sample)
) STRICT;
CREATE INDEX speech_receipts_meeting_idx ON speech_receipts(meeting_id);

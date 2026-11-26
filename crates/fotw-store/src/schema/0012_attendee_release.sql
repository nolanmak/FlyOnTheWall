-- Attendee release (#326, #331). Every table here is device-local: none is
-- carried by a library archive or by sync (D17, N9). The worker owns them.

-- One release per meeting (5.3). Deleted with its meeting.
CREATE TABLE meeting_releases (
  meeting_id TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
  state TEXT NOT NULL CHECK(state IN
    ('preparing', 'needs_review', 'scheduled', 'sending', 'held', 'sent', 'dropped')),
  -- Every applicable reason code, as a JSON array (D10). While `preparing`,
  -- the reasons the finished preparation must keep (`edited`, `inputs_changed`).
  reasons_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(reasons_json)),
  source TEXT NOT NULL,
  source_event_id TEXT NOT NULL,
  ical_uid TEXT,
  -- Frozen at creation (D4); never updated.
  event_title TEXT NOT NULL,
  recipients_json TEXT NOT NULL CHECK(json_valid(recipients_json)),
  end_index INTEGER CHECK(end_index IS NULL OR end_index >= 0),
  omit_ranges_json TEXT CHECK(omit_ranges_json IS NULL OR json_valid(omit_ranges_json)),
  reviewer_retained_json TEXT
    CHECK(reviewer_retained_json IS NULL OR json_valid(reviewer_retained_json)),
  summary_json TEXT CHECK(summary_json IS NULL OR json_valid(summary_json)),
  rendered_subject TEXT,
  rendered_body TEXT,
  rendered_attachment TEXT,
  -- `<YYYY-MM-DD>-<slug>.md`, the name `gws` gives the attachment (D9).
  rendered_attachment_name TEXT,
  index_map_json TEXT CHECK(index_map_json IS NULL OR json_valid(index_map_json)),
  findings_json TEXT CHECK(findings_json IS NULL OR json_valid(findings_json)),
  input_fingerprint TEXT,
  engine_id TEXT,
  approved_by TEXT CHECK(approved_by IS NULL OR approved_by IN ('human', 'auto')),
  send_at INTEGER,
  sent_to_json TEXT CHECK(sent_to_json IS NULL OR json_valid(sent_to_json)),
  sent_at INTEGER,
  message_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX meeting_releases_state_idx ON meeting_releases(state, send_at);

-- Why a meeting has no release row yet, re-evaluated at most every 15
-- minutes inside the 7-day window (D10). A cache; deleted with its meeting.
CREATE TABLE release_eligibility (
  meeting_id TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
  reason TEXT NOT NULL,
  evaluated_at INTEGER NOT NULL
) STRICT;

-- The evidence the auto unlock reads (D11, D19). Append-only, and with no
-- foreign key on purpose: deleting a meeting must not change the unlock.
CREATE TABLE release_decisions (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL,
  engine_id TEXT NOT NULL,
  decision TEXT NOT NULL CHECK(decision IN
    ('approved_unchanged', 'approved_edited', 'dropped', 'auto', 'sent_again')),
  at_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX release_decisions_engine_idx ON release_decisions(engine_id, decision);
CREATE TRIGGER release_decisions_append_only BEFORE UPDATE ON release_decisions BEGIN
  SELECT RAISE(ABORT, 'release_decisions is append-only');
END;

-- Every address a release has gone to, for the first-time-recipient gate
-- (D19). Lowercase; not tied to any meeting.
CREATE TABLE release_sent_addresses (
  address TEXT PRIMARY KEY CHECK(address = lower(address)),
  first_sent_ms INTEGER NOT NULL
) STRICT;

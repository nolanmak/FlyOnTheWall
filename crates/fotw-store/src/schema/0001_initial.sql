-- Migration 0001 -- the initial schema (docs/REQUIREMENTS.md 9.3).
--
-- House rules, every one of them load-bearing and every one of them tested in
-- crates/fotw-store/tests/schema.rs:
--
--   * every table is STRICT, so a typo that stores '12' in an INTEGER column
--     fails at the INSERT instead of surfacing months later as a sort that
--     puts '10' before '9';
--   * every primary key is TEXT holding a UUIDv7 (§9.7 invariant 1);
--   * every timestamp is INTEGER Unix epoch MILLISECONDS UTC, and is named
--     `*_at` or `*_ms` so the unit is visible at the call site;
--   * every mutable row carries updated_at + lamport + origin_device_id, and
--     merge order is (lamport, origin_device_id) -- never wall clock, because
--     clock skew between a user's two laptops is real (§9.7 invariant 2);
--   * every path column is RELATIVE to the app data root (§9.7 invariant 5),
--     which is why they are all named `*_rel_path`.
--
-- NOTE ON auto_vacuum: §9.1 says `PRAGMA auto_vacuum = INCREMENTAL` belongs
-- here, before the first CREATE TABLE. It is issued from Rust in
-- `Db::bootstrap` instead, because rusqlite_migration wraps the whole
-- migration run in a single transaction and `PRAGMA auto_vacuum` is a
-- documented no-op inside one. The requirement (set it before any table
-- exists) is met either way, and `tests/bootstrap.rs` asserts the result.

------------------------------------------------------------------- provenance

-- Devices that have ever written to this library. `is_self` marks this
-- install; everything that writes stamps its id into origin_device_id.
CREATE TABLE devices (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL DEFAULT '',
  platform TEXT NOT NULL,                        -- macos|windows|linux
  app_version TEXT NOT NULL DEFAULT '',
  is_self INTEGER NOT NULL DEFAULT 0,
  last_seen_at_ms INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;
-- Exactly one row may claim to be this machine.
CREATE UNIQUE INDEX devices_self_uidx ON devices(is_self) WHERE is_self = 1;

-- Facts about the library itself (schema provenance, recovery-key state, the
-- app version that last wrote). Keyed by a well-known name rather than a
-- UUIDv7: these are not identities that two devices could ever both mint, so
-- §9.7 invariant 1 does not apply.
CREATE TABLE app_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- User preferences. Same keyed-by-name reasoning as app_meta, but these DO
-- sync, so they carry the merge triple.
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,                           -- JSON scalar or document
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;

------------------------------------------------------------------ organisation

CREATE TABLE folders (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  parent_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
  sort_key TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE INDEX folders_parent_idx ON folders(parent_id);

-- Summary templates. `body_md` is the template body that §8.3 sandwiches
-- between the immutable grounding contract and the immutable footer.
CREATE TABLE templates (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  body_md TEXT NOT NULL,
  is_builtin INTEGER NOT NULL DEFAULT 0,
  is_default INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX templates_default_uidx ON templates(is_default) WHERE is_default = 1;

CREATE TABLE people (
  id TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  email TEXT,
  is_self INTEGER NOT NULL DEFAULT 0,
  -- The diarisation label ('Speaker 1') this person has been bound to. Kept
  -- per-person rather than per-segment so a correction applies retroactively.
  voice_label TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX people_email_uidx ON people(email) WHERE email IS NOT NULL;

---------------------------------------------------------------------- meetings

-- There is deliberately NO meetings.summary_text column. See `summaries`.
CREATE TABLE meetings (
  id TEXT PRIMARY KEY,
  title TEXT NOT NULL DEFAULT '',
  started_at_ms INTEGER NOT NULL,
  ended_at_ms INTEGER,
  duration_ms INTEGER,
  tz TEXT NOT NULL,                              -- IANA name, e.g. Europe/Berlin
  folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
  template_id TEXT REFERENCES templates(id) ON DELETE SET NULL,
  calendar_uid TEXT,
  calendar_source TEXT,
  meeting_url TEXT,
  app_hint TEXT,
  state TEXT NOT NULL,                           -- recording|transcribing|ready|failed
  language TEXT,
  -- Consent is a first-class field, not a UI flag: §11 turns this into the
  -- disclosure banner and the export footer.
  disclosed INTEGER NOT NULL DEFAULT 0,
  retain_audio TEXT NOT NULL DEFAULT 'default',  -- default|forever|until_transcribed|days
  retain_audio_days INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE INDEX meetings_started_idx ON meetings(started_at_ms DESC);
CREATE INDEX meetings_folder_idx ON meetings(folder_id);
CREATE INDEX meetings_state_idx ON meetings(state);

CREATE TABLE meeting_participants (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  person_id TEXT REFERENCES people(id) ON DELETE SET NULL,
  -- Snapshots, because calendar display names drift and a meeting should
  -- render the way it did on the day.
  display_name TEXT NOT NULL,
  email TEXT,
  role TEXT,                                     -- organizer|attendee|unknown
  speaker_label TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL,
  -- NULL person_id rows are distinct under SQLite's UNIQUE semantics, which
  -- is what we want: several unmatched attendees per meeting is normal.
  UNIQUE (meeting_id, person_id)
) STRICT;
CREATE INDEX meeting_participants_person_idx ON meeting_participants(person_id);

------------------------------------------------------------------- transcripts

-- Multiple transcripts per meeting is deliberate: re-transcribing with a
-- different provider must not destroy the old one. `is_primary` names the one
-- the UI shows, and the partial unique index makes "exactly one" a database
-- invariant rather than an application convention.
CREATE TABLE transcripts (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  is_primary INTEGER NOT NULL DEFAULT 1,
  language TEXT,
  audio_ms INTEGER,
  cost_micros INTEGER,
  -- media/<yyyy>/<mm>/<id>/raw-<provider>.json.zst, relative to the data root.
  -- This file holds the full provider response and is the artifact §9.6 calls
  -- the most commonly forgotten one on delete.
  raw_response_rel_path TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX transcripts_primary_uidx ON transcripts(meeting_id) WHERE is_primary = 1;
CREATE INDEX transcripts_meeting_idx ON transcripts(meeting_id);

-- `words` is a zstd'd JSON blob, not rows. A heavy user generates ~8.4M words
-- a year; as rows that is an 8.4M-row table that no query ever selects a
-- single row from. As per-segment BLOBs it is ~80 KB per meeting, decompressed
-- lazily only when a segment is played back (§9.3).
--
-- meeting_id is denormalised here on purpose. It gives segments a direct
-- CASCADE edge to meetings (so the §9.6 delete cannot be defeated by an
-- orphaned transcript) and lets the timeline query hit
-- segments_meeting_time_idx without joining transcripts.
CREATE TABLE segments (
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
  UNIQUE (transcript_id, idx)
) STRICT;
CREATE INDEX segments_meeting_time_idx ON segments(meeting_id, start_ms);

------------------------------------------------------------------------- notes

-- §9.7 invariant 3: body_md is the ONE column two devices would both rewrite,
-- and is therefore the first thing that becomes a CRDT if sync ever ships.
-- Keep it the only user-content column on this table. One live note document
-- per meeting, enforced by the UNIQUE, so the editor can upsert by meeting id.
CREATE TABLE notes (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  body_md TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL,
  UNIQUE (meeting_id)
) STRICT;

-- THE alignment table. Every markdown block remembers when it was typed, which
-- is what turns "expand my notes" from a summarisation prompt into a
-- time-windowed lookup: the transcript around typed_at_ms is what the user was
-- reacting to. Without this the augmented-notes feature is just summarisation.
CREATE TABLE note_anchors (
  id TEXT PRIMARY KEY,
  note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  block_idx INTEGER NOT NULL,
  -- Snapshot of the block as it stood, used to re-anchor after later edits
  -- shift block indices around.
  block_text TEXT NOT NULL,
  -- Milliseconds from meeting start (NOT epoch): the first keystroke in this
  -- block. Relative because it has to survive the meeting's start time being
  -- corrected after the fact.
  typed_at_ms INTEGER NOT NULL,
  UNIQUE (note_id, block_idx)
) STRICT;
CREATE INDEX note_anchors_meeting_time_idx ON note_anchors(meeting_id, typed_at_ms);

--------------------------------------------------------------------- summaries

-- Append-only versioned rows. There is deliberately no meetings.summary_text:
-- that single mutable cell is the one thing that would make future sync a
-- merge nightmare, and it also throws away the previous summary every time a
-- user regenerates with a different template.
CREATE TABLE summaries (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  template_id TEXT REFERENCES templates(id) ON DELETE SET NULL,
  transcript_id TEXT REFERENCES transcripts(id) ON DELETE SET NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  -- sha256 of the fully rendered prompt. With the versioned prompt file from
  -- §8.3 this is what makes a regeneration reproducible and diffable.
  prompt_hash TEXT NOT NULL,
  body_md TEXT NOT NULL,
  coverage REAL,                                 -- §8.6 citation-coverage metric
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_micros INTEGER,
  is_current INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  origin_device_id TEXT NOT NULL,
  UNIQUE (meeting_id, version)
) STRICT;
CREATE UNIQUE INDEX summaries_current_uidx ON summaries(meeting_id) WHERE is_current = 1;

-- The §8.5 extraction schema, landed. Nullable owner/due are load-bearing:
-- they are the mechanism that lets the model decline to invent, so they must
-- stay nullable in the DB too. evidence_segment_ids is JSON because it is a
-- validation input, never a join target.
CREATE TABLE action_items (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  summary_id TEXT REFERENCES summaries(id) ON DELETE CASCADE,
  kind TEXT NOT NULL DEFAULT 'action_item',      -- action_item|decision|open_question|follow_up
  text TEXT NOT NULL,
  owner_person_id TEXT REFERENCES people(id) ON DELETE SET NULL,
  owner_label TEXT,                              -- exact speaker name, or NULL. Never a guess.
  due_ms INTEGER,                                -- resolved ISO-8601 -> epoch ms, or NULL
  due_raw TEXT,                                  -- the literal phrase, e.g. 'end of next sprint'
  confidence TEXT NOT NULL DEFAULT 'implied',    -- explicit|implied
  evidence_segment_ids TEXT NOT NULL DEFAULT '[]', -- JSON array of segments.id
  evidence_quote TEXT,
  status TEXT NOT NULL DEFAULT 'open',           -- open|done|dismissed
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE INDEX action_items_meeting_idx ON action_items(meeting_id);
CREATE INDEX action_items_status_idx ON action_items(status) WHERE status = 'open';

-------------------------------------------------------------------------- tags

CREATE TABLE tags (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  color TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX tags_name_uidx ON tags(name);

-- Composite PK: the pair IS the identity, so there is nothing here for two
-- devices to mint differently and no UUID is needed.
CREATE TABLE meeting_tags (
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL,
  PRIMARY KEY (meeting_id, tag_id)
) STRICT;
CREATE INDEX meeting_tags_tag_idx ON meeting_tags(tag_id);

-------------------------------------------------------------------- recordings

-- One row per (meeting, channel). rel_path is relative to the data root so the
-- whole folder can be moved or restored onto another machine (§9.7 inv. 5).
-- `state` and `purge_after_ms` drive the §9.5 retention sweeper; `deleted_at`
-- keeps the row after the bytes are gone so the UI can say "audio was deleted
-- on <date>" rather than pretending the meeting never had any.
CREATE TABLE recordings (
  id TEXT PRIMARY KEY,
  meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  channel TEXT NOT NULL,                         -- mic|system
  rel_path TEXT NOT NULL,
  codec TEXT NOT NULL DEFAULT 'opus',
  container TEXT NOT NULL DEFAULT 'ogg',
  sample_rate INTEGER NOT NULL DEFAULT 48000,
  channels INTEGER NOT NULL DEFAULT 1,
  bitrate_bps INTEGER NOT NULL DEFAULT 24000,
  duration_ms INTEGER,
  bytes INTEGER,
  sha256 TEXT,
  encrypted INTEGER NOT NULL DEFAULT 1,
  state TEXT NOT NULL,                           -- writing|complete|deleted
  purge_after_ms INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  lamport INTEGER NOT NULL DEFAULT 0,
  origin_device_id TEXT NOT NULL,
  UNIQUE (meeting_id, channel)
) STRICT;
CREATE INDEX recordings_purge_idx ON recordings(purge_after_ms) WHERE state = 'complete';

-------------------------------------------------------------------- tombstones

-- Identity is remembered, content is destroyed. NEVER any content here: not a
-- title, not a snippet, not a path. A tombstone exists so a future sync can
-- tell "deleted" from "never seen", and nothing else. §9.6's byte-scan
-- acceptance test is what keeps this honest.
CREATE TABLE tombstones (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,                            -- meeting|note|summary|...
  deleted_at INTEGER NOT NULL,
  origin_device_id TEXT NOT NULL,
  lamport INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX tombstones_kind_time_idx ON tombstones(kind, deleted_at);

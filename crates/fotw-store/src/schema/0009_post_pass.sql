-- The post-meeting cleanup pass (#246). One job per meeting re-transcribes its
-- saved audio into a new transcript version beside the live one. A window's
-- words are checkpointed as soon as it is transcribed, so a restarted daemon
-- resumes from the last finished window; the checkpoints are deleted with the
-- job's completion, in the same transaction that writes the new version.
CREATE TABLE post_pass_jobs (
  meeting_id TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
  state TEXT NOT NULL CHECK(state IN ('pending', 'done', 'failed')),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  -- What the job reads and how: tracks, window sizes, engines and revisions.
  -- A resumed job refuses checkpoints made under a different plan.
  plan_json TEXT NOT NULL CHECK(json_valid(plan_json)),
  progress_permille INTEGER NOT NULL DEFAULT 0
    CHECK(progress_permille BETWEEN 0 AND 1000),
  live_transcript_id TEXT REFERENCES transcripts(id) ON DELETE SET NULL,
  cleaned_transcript_id TEXT REFERENCES transcripts(id) ON DELETE SET NULL,
  -- Engines, revisions and what was skipped, as the export shows them.
  provenance_json TEXT CHECK(provenance_json IS NULL OR json_valid(provenance_json)),
  detail TEXT
) STRICT;
CREATE INDEX post_pass_jobs_state_idx ON post_pass_jobs(state);

CREATE TABLE post_pass_windows (
  meeting_id TEXT NOT NULL REFERENCES post_pass_jobs(meeting_id) ON DELETE CASCADE,
  channel TEXT NOT NULL CHECK(channel IN ('mic', 'system')),
  idx INTEGER NOT NULL CHECK(idx >= 0),
  words_json TEXT NOT NULL CHECK(json_valid(words_json)),
  PRIMARY KEY (meeting_id, channel, idx)
) STRICT;

-- Migration 0003 -- what enrichment found, kept where the product can show it.
--
-- Issue #74: every one of the 33 meetings in the real library had
-- `summary_md: NULL`, and "no engine configured", "engine configured but the
-- daemon cannot resolve its binary" and "the engine ran and failed" all
-- rendered as the same blank space. The daemon knew which of the three had
-- happened every time; it printed it to a stderr a LaunchServices `.app`
-- discards. These two columns are where it says so instead.
--
-- Two statements rather than one: SQLite's ALTER TABLE takes exactly one
-- ADD COLUMN at a time.
--
-- Nullable with no default, because NULL is a real and distinct state -- this
-- meeting has never been through a build that could report -- and a default of
-- 'unknown' would make the 33 stranded meetings indistinguishable from a
-- meeting enriched a second ago by a broken engine.
--
-- `enrich_status` is one of: ok | no_engine | engine_unresolvable | failed.
-- `enrich_detail` is the reason, in words, for the two that have one: the
-- configured-but-unresolvable binary name, or the CLI's own first complaint.
-- It is **untrusted text** -- it carries child-process stderr -- so every
-- renderer puts it in the DOM through `textContent` (ING-11).
--
-- Derived, device-local state: which binary *this* machine can resolve says
-- nothing about the user's other laptop. `MeetingRepo::set_enrich_report`
-- therefore deviates from §9.7 invariant 2 and does not bump
-- `updated_at`/`lamport`; see the note beside the mutations that do.

ALTER TABLE meetings ADD COLUMN enrich_status TEXT;
ALTER TABLE meetings ADD COLUMN enrich_detail TEXT;

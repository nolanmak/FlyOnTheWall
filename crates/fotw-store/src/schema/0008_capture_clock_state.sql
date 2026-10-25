-- An interrupted recording can be indexed before its final clock is proven.
-- For these aliases elapsed_ms is an observed high-water mark, never an end.
ALTER TABLE session_note_links ADD COLUMN clock_is_final INTEGER NOT NULL DEFAULT 1
  CHECK(clock_is_final IN (0, 1));

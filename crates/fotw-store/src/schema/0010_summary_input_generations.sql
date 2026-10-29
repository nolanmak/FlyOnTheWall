-- Local concurrency generations, not user data or authorization tokens.
-- Every committed input mutation allocates a new opaque generation, including
-- edits away and back. Delete/recreate and independent libraries get new values.
-- Archive export/import excludes this reserved app_meta namespace; importing
-- source rows rebuilds fresh generations through these triggers.
INSERT INTO app_meta(key,value,updated_at)
  SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings;

CREATE TRIGGER summary_input_meeting_insert AFTER INSERT ON meetings BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_meeting_delete AFTER DELETE ON meetings BEGIN
  DELETE FROM app_meta WHERE key='summary_input_epoch:'||OLD.id;
END;

-- Titles/context are inputs to later generation phases. A generated title
-- publishes once, and the next phase captures the new generation. Generated
-- summaries and sharing documents do not mutate these source inputs.
CREATE TRIGGER summary_input_meeting_context AFTER UPDATE OF title,started_at_ms,ended_at_ms,duration_ms,tz ON meetings BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_transcripts_insert AFTER INSERT ON transcripts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_transcripts_update AFTER UPDATE ON transcripts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_transcripts_delete AFTER DELETE ON transcripts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_segments_insert AFTER INSERT ON segments BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_segments_update AFTER UPDATE ON segments BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_segments_delete AFTER DELETE ON segments BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_notes_insert AFTER INSERT ON notes BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_notes_update AFTER UPDATE ON notes BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_notes_delete AFTER DELETE ON notes BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_note_anchors_insert AFTER INSERT ON note_anchors BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_note_anchors_update AFTER UPDATE ON note_anchors BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_note_anchors_delete AFTER DELETE ON note_anchors BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_epochs_insert AFTER INSERT ON speech_epochs BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_epochs_update AFTER UPDATE ON speech_epochs BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_epochs_delete AFTER DELETE ON speech_epochs BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_receipts_insert AFTER INSERT ON speech_receipts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_receipts_update AFTER UPDATE ON speech_receipts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_speech_receipts_delete AFTER DELETE ON speech_receipts BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_session_note_links_insert AFTER INSERT ON session_note_links BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_session_note_links_update AFTER UPDATE ON session_note_links BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id,NEW.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;

CREATE TRIGGER summary_input_session_note_links_delete AFTER DELETE ON session_note_links BEGIN
  INSERT INTO app_meta(key,value,updated_at)
    SELECT 'summary_input_epoch:'||id,lower(hex(randomblob(32))),0 FROM meetings
    WHERE id IN (OLD.meeting_id)
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at;
END;


-- Purpose-aware sharing drafts are derived, versioned, and deleted with their meeting.
CREATE TABLE meeting_documents (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    document_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (meeting_id, version)
) STRICT;

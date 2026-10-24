# Notes editor progress (#122 / #13)

Saved meetings have an editable textarea with 500 ms debounce, a 5-second
maximum debounce, blur/navigation flush, and explicit save/retry. Each draft
owns its meeting ID. Navigation leaves unfinished writes associated with that
ID; edits made during a save are queued behind it using the returned revision.

Draft text stays in tab memory. Unloading with unsaved text warns the user.
No note text is written to browser storage. This does not provide recovery
from a browser crash: an encrypted draft/recovery path is still required.

GET/POST `/api/meetings/{id}/notes` use the existing host/origin/bearer guard.
The request contains `revision`, `body_md`, and `anchors`; revision zero means
no saved note. A stale revision returns 409 and leaves both body and anchors
unchanged. The UI offers explicit discard/reload or replacement with the user's
own draft, never automatic conflict overwrite. Failed writes remain unsaved.
The backing store uses an immediate SQLite transaction for revision comparison,
text replacement, and anchor replacement. The existing Lamport counter means
legacy note writes invalidate editor snapshots too, without a schema rewrite.

Store limits: 1 MiB UTF-8 body, 1 MiB aggregate anchor text, at most 4,096
anchors, nonnegative recording offsets and unique nonnegative block positions.
The HTTP JSON request also has Axum's default 2 MiB body limit. Invalid saves
preserve the preceding document. Store errors never expose note content in API
responses. Search, meeting details, and Markdown/JSON export read the same note.
Typing does not run a summarizer or change a transcript.

Tests exercised real failing behavior before implementation: stale-tab overwrite,
invalid-save preservation, missing API route, lost edits during an in-flight
save, network failure, and conflict resolution. Temporary encrypted databases
cover two connections, export and search. An injected anchor-insert failure
proves rollback of text, revision, anchors and the FTS index. UI tests use literal script-shaped
text in the textarea. The store/web regression suites and workspace Clippy pass;
52 Node UI tests pass. Full workspace regression is also run before pushing.

Outstanding before completion: live-session identity handoff; encrypted draft
recovery across browser/daemon restarts; stable block identities and first-key
anchors through edits/reordering; actual browser keyboard/narrow-layout checks.
The current saved-meeting editor retains supplied anchors without creating
historic timestamps. Anchor remapping through text edits remains #13 work.
Neither #122 nor #13 is complete.

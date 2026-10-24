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
Anchor IDs are unique, nonempty ASCII identifiers of at most 64 bytes.
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
recovery across browser/daemon restarts; editor block tracking and first-key
anchors through edits/reordering; actual browser keyboard/narrow-layout checks.
The saved-meeting editor remaps supplied anchors without creating historic
timestamps. Live first-keystroke capture and browser acceptance remain #13 work.
Neither #122 nor #13 is complete.

## Persistent block identity

The existing `note_anchors.id` now survives store/API roundtrips instead of being
regenerated at every save. No database migration is required. Both note writers
preserve supplied identities and reject moving an existing identity to another
meeting or changing its original recording offset. The revision-checked editor
writer additionally rejects duplicate, empty or oversized IDs before mutation.
The API returns `id` on every anchor; older clients may omit it to create an
anchor, and must retain the returned ID for subsequent edits. Existing stored
rows gain no new timestamps or IDs when loaded.

A regression test first demonstrated that editing/reordering changed the stored
ID. Store and HTTP tests now preserve IDs/offsets across text/index updates and
verify atomic rejection of changed offsets, duplicate identities and identity
reuse from another meeting. Existing export/import and store regression tests
pass. These guarantees apply to caller-supplied block edits; the editor integration below tracks supported block operations. No claim of
complete live recording-time capture or browser acceptance is made.

## Editor block operations

The textarea identifies paragraphs, list items, headings, individual quoted lines
and fenced code blocks. It preserves the original Markdown during typing; blank
lines inside a fence do not split the block. Anchors follow unchanged snapshots
through reorder, and in-place typing updates the snapshot while keeping its ID
and original time. Native `beforeinput` selection evidence distinguishes edits
or deletions of otherwise identical paragraphs. A split keeps the leading
block's original anchor; a merge keeps the leading surviving anchor. Deleting a
block removes its anchor; retyping or pasting a new block into a saved meeting
does not invent a historical time.

“Move block up/down” moves the block containing the cursor, including its ID,
even when neighboring blocks have identical text. Movement separates blocks with
blank lines while preserving their internal text, and restores focus/selection.
Use these controls for an unambiguous move: deleting through cut then pasting is
treated as deletion and creation. A whole-document replacement that cannot be
mapped to surviving blocks drops ambiguous anchors rather than attaching them
to unrelated text. The block grammar is intentionally the editor's documented
subset, not a complete CommonMark parser.

Red/green tests cover edits, reorder, insertion, deletion/recreation, duplicate
paragraph edits/deletion, split/merge, fenced code, Unicode, explicit identical
block movement and matching text/anchor snapshots during in-flight saves. A
mounted-control test verifies keyboard focus and the emitted save payload. All
63 UI tests pass; actual browser/IME, mobile selection and live recording-time
checks remain outstanding.

## Encrypted recording drafts (storage foundation)

Schema 5 adds a STRICT `session_notes` table keyed by the capture/WAL session ID,
before the recording has a meeting row. `SessionNoteDocument` stores the original
capture start, revision, Markdown and stable anchors in the SQLCipher library.
The save API uses an immediate transaction and compare-and-swap revision. It
shares the note size/identity validation, rejects changing the capture start or
an existing anchor's original time, and rejects anchors beyond the supplied
recording elapsed time. That elapsed time must come from the authoritative
recorder when the HTTP adapter is added, not from a client assertion.

A forward-only migration preserves existing note text, revisions and anchor IDs.
Explicit library archives include the draft rows; their existing plaintext
warning applies. Import still accepts pre-schema-5 manifests without drafts.
Tests cover reopen recovery, ciphertext lacking a fixture plaintext marker,
stale revisions, bounds, unchanged offsets, injected update failure, full archive
roundtrips (including every column) and migration from a populated v4 library.

This is not yet user-visible crash recovery: the live notes routes/editor,
recovery discovery and daemon integration of the handoff contract below still
need implementation. No new browser storage is used. The table is not a
replacement for that complete flow, and #122/#13 remain open.

## Atomic handoff and late saves (store contract)

Schema 6 adds a session-to-meeting alias with the original capture start and
frozen elapsed time. `promote_session_note` moves text and anchors, preserves the
note revision, creates the alias and removes the draft in one transaction. An
existing destination note is a conflict; a repeated handoff to the same meeting
and clock is idempotent. An alias is created even when the first debounced note
save has not arrived yet. Reads and later saves through the session then operate
on the canonical meeting note with the same compare-and-swap revision. A saved
meeting editor and an old live tab therefore cannot silently overwrite each
other, and caller-supplied elapsed time cannot exceed the frozen recording bound.
Deleting a meeting cascades its alias.

Tests cover draft revision continuity, first save after Stop, concurrent editor
conflicts across connections, idempotence, frozen bounds and rollback after an
injected alias-insert failure (including text, anchors and FTS). The aliases are
included in full library archive roundtrips; older manifests remain readable.
A populated schema-5 draft survives the forward migration.

The daemon and HTTP/live editor still need to call this contract with the actual
capture-stop clock. Sample counts and the later persistence timestamp are not
substitutes, especially with gaps or a long transcription drain. This store work
does not yet establish end-to-end live notes or crash recovery UX.

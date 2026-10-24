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
62 Node UI tests pass. Full workspace regression is also run before pushing.

Outstanding before completion: live-session identity handoff; encrypted draft
recovery across browser/daemon restarts; live first-key anchors; trusted
keyboard/IME and mobile selection checks.
The saved-meeting editor remaps supplied anchors without creating historic
timestamps. Live first-keystroke capture remains #13 work.
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
complete live recording-time capture is made; saved-editor rendering is checked below.

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
62 UI tests pass. Trusted keyboard/IME, mobile selection and live recording-time
checks remain outstanding.

## Saved-editor WebKit acceptance

An isolated WebKit view with a nonpersistent data store loaded the real embedded
assets and API over the synthetic `ui_preview` library. It saved literal
script-shaped text, observed a higher revision and Saved status, switched to a
different meeting without leaking the draft, and returned to the saved text.
Six fresh WebKit views recovered it from the encrypted store at 390, 768 and
1440 CSS pixels in both light and dark appearances. All six reported no page
horizontal overflow, a labeled/focusable textarea and respected hidden states.
The initial rendering exposed the hidden consent label; the global hidden rule
fixes author styles overriding that attribute. Screenshots were inspected.

These are DOM/API and rendering checks, including synthetic input events.
They do not establish trusted hardware keyboard events, IME behavior, real
recording timestamps or crash recovery of unsaved drafts. No user meeting,
provider key, microphone or browser profile was used. The view minted a fresh
one-time launch URL for each run, respecting the 30-second handoff lifetime.

## Durable recording drafts and atomic handoff (storage increment)

Schema 5 adds `session_notes`, keyed by capture session before a meeting row
exists. Acknowledged drafts store their original start, revision, Markdown and
anchors inside the encrypted library. Saves compare the observed revision in an
immediate transaction, preserve original anchor times and validate bounds against
an elapsed time supplied by the recorder. Reopening the database retains drafts.

Schema 6 adds a session-to-meeting alias with the original start and frozen
recording duration. Promotion moves text/anchors, preserves the revision, creates
the alias and removes the draft atomically. Existing destination notes cause a
conflict. Repeating the same promotion is idempotent. The alias also covers a
first save arriving after Stop: session reads/writes then address the canonical
meeting note, so a stale live tab and saved-meeting editor cannot overwrite one
another silently. Deleting the meeting cascades the alias.

Focused encrypted-database tests cover restart recovery, stale revisions,
recording/anchor bounds, cross-connection conflicts, late saves, first save after
Stop, and rollback after injected write/alias failures. Schema migrations preserve
populated older libraries, full archives round-trip both new tables, and imports
continue to accept older manifests without these rows. Explicit archives retain
their existing plaintext-export warning.

This increment provides the store contract. The daemon and authenticated live
routes still need to supply the authoritative capture/stop clock and call the
handoff. Recovery discovery and the live editor remain required before claiming
user-visible recording-draft recovery. Unacknowledged keystrokes are not covered
by database durability. No browser storage is added; #122 and #13 stay open.

## Recording finalization integration

Recorder status now uses the original capture/WAL start clock. The first stop
request freezes its end before tap teardown, transcription drain or persistence,
and later stop/cleanup calls cannot extend it. `SessionOutcome` carries that
boundary so meeting duration excludes final transcription time. Legacy imported
outcomes without a stop boundary keep their existing fallback behavior.

For outcomes with an authoritative boundary, persistence commits the meeting,
transcript, note promotion and session alias in one transaction. A failed handoff
leaves the draft and no phantom meeting; retry uses the same session identity.
The WAL reserves directories with atomic creation and retries collisions rather
than reopening an existing session directory.

Fixture tests cover frozen clocks, nonnegative bounds, capture-end persistence,
transaction rollback/retry and preservation of files under colliding identities.
These clocks use epoch milliseconds with rollback clamping; physical clock,
sleep/wake and device behavior still need native acceptance. The authenticated
live-note routes and UI remain the next integration step for #122/#13.

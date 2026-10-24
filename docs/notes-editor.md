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
63 Node UI tests pass. Full workspace regression is also run before pushing.

Outstanding before completion: encrypted draft recovery discovery across
browser/daemon restarts; live recording acceptance; actual browser
keyboard/IME/narrow-layout checks.
The saved-meeting editor remaps supplied anchors without creating historic
timestamps. Live first-input capture is implemented below; browser acceptance remains #13 work.
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
recording elapsed time. The HTTP adapter obtains that bound from the recorder
or the finalized session alias, never from a client assertion.

A forward-only migration preserves existing note text, revisions and anchor IDs.
Explicit library archives include the draft rows; their existing plaintext
warning applies. Import still accepts pre-schema-5 manifests without drafts.
Tests cover reopen recovery, ciphertext lacking a fixture plaintext marker,
stale revisions, bounds, unchanged offsets, injected update failure, full archive
roundtrips (including every column) and migration from a populated v4 library.

This is not yet user-visible crash recovery: recovery discovery still needs
implementation. No new browser storage is used. The table is not a
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

The daemon now finalizes capture through one transaction for the meeting,
transcript, frozen duration and draft handoff. A retry with the same session and
clock returns the existing meeting without overwriting later edits. Failures
roll back newly created rows and leave the draft intact. The start comes from
the WAL and the end is frozen before transcription drains. Legacy imports
without a capture clock retain their independent-import behavior.


## Live-note HTTP contract

GET/POST `/api/sessions/{id}/notes` use the same host/origin/bearer guard and
request document as saved notes. Responses contain `session_id`, `meeting_id`
(null until finalized), `started_at_ms`, `elapsed_ms` and `note`. The recorder
provides the clock only when its current session ID matches the route; a
persisted alias takes precedence after finalization, even if another recording
has started. Clock fields in a POST body are rejected.

A durable orphan draft can be read by ID with `elapsed_ms: null`. Writing it
without an active or finalized clock returns 409 with
`recording_clock_unavailable`; clients must retain their draft and retry after
finalization instead of treating this as a revision conflict. An unknown session
returns the standard bare 404, including during the finalization gap if no draft
has ever been saved. Once an alias exists, the first save is accepted even if no
live draft existed. Revisions, original anchor offsets and the frozen elapsed
bound are enforced by the store on every late write.

The synthetic HTTP test first failed on the missing authenticated route, then
passed authentication, hostile-origin, forged-clock, wrong-session, bounds,
stale-write, orphan-read, finalization-gap and first/late-save checks against a
temporary encrypted library. Recovery discovery and real browser acceptance remain outstanding; these
endpoints alone do not complete #122 or #13.


## Live editor and capture clock

The recording pane now mounts the editor once per capture ID. Status polls and
transcript updates do not rebuild it. Each draft retains its session endpoint;
when the saved identity arrives, navigation reuses that draft, including pending
writes. An independently edited saved-meeting draft is never silently replaced.
Stop and navigation flush pending text, and a persisted-session event retries a
save that arrived during the finalization gap. No note text enters browser storage.

A newly created Markdown block receives an ID and recording offset synchronously
in the input handler, before the 500 ms autosave timer. Further typing preserves
that first time. Paste creates each new block at the same input time; splitting
keeps the leading identity and gives a new trailing block the split time.
Deleting then recreating creates a new identity/time. Existing unanchored text
loaded from storage stays unanchored when edited. After Stop or a saved identity,
new text has no historical recording anchor.

The clock interpolates from the recorder's elapsed time, accounts for elapsed
wall time across sleep, clamps a backward wall-clock jump, and refuses anchors
at or beyond the announced automatic stop. Stop disables new anchors before its
HTTP request; an in-flight recording poll cannot re-enable them. An old draft
cannot use a new session's clock. If finalization reveals that newly typed blocks
fall beyond the real capture end, retry retains their text and removes those
out-of-bounds anchors. Metadata refresh never adopts a newer note revision to
bypass conflict checking.

Behavioral red/green tests covered the missing live mount, missing first-input
anchor and absent capture-clock interpolation. The 70 UI tests now include
live/saved navigation during a pending write, first-input timing, paste, edit,
reorder, deletion/recreation, Stop, automatic bounds, clock rollback, finalization
retry and preserving text after a late end-clock update. Real browser/IME,
permission/hardware tests and encrypted orphan discovery remain separate gates.


Native Chrome check (synthetic library, 2026-10-24): edited a saved note using
macOS Accessibility input, observed Saved, navigated to another meeting without
leaking the fixture text, returned to the original note, then reloaded the page
and confirmed the saved fixture survived. A native keyboard path was used for
meeting navigation. This check also caught a component display rule overriding
`hidden` on the read-only consent label; a global `[hidden]` rule fixes it, and
the rebuilt preview's accessibility tree confirms both consent and Start are
absent while the library still renders. Test tabs, servers and temporary
libraries were removed. CDP was unavailable and no browser settings changed.
This establishes saved-note editing/navigation/reload, not live hardware capture,
IME behavior or narrow-layout acceptance.


## Transcript windows for augmentation

The daemon reads the note body and anchor snapshot from the same meeting as its
primary transcript. Each anchored block supplies a window from 90 seconds before
to 90 seconds after its original capture offset, clipped to zero and the saved
meeting duration. First-speech display rebasing never enters this calculation.
A transcript segment intersecting the window keeps its whole text and original
timestamps; without word-level alignment, trimming its text would fabricate a
boundary. A block outside the recording duration receives no contextual evidence.

The augment call receives raw notes plus structured block windows and matching
excerpts. Overlapping windows share excerpt text once, referenced by original
segment IDs. In map/reduce, each request includes only excerpts available in its
own transcript chunk. The instruction treats excerpts as untrusted speech,
requires matching-window grounding and citations to the transcript document,
and forbids inventing evidence for an empty window. Unanchored notes retain their
existing raw-note input path. Typing still does not trigger a summary call.

Before any anchored-note provider call, the pipeline checks all planned chunks
against context capacity, including output reserve and a conservative byte-based
ceiling for structured note content. It repacks into smaller whole-speaker-turn
chunks when necessary. A note payload or indivisible turn that cannot fit returns
a context error before making a call. Only one chunk's repeated window payload
is allocated at a time. This is conservative estimation, not a model tokenizer
or a token-cost measurement.

Red/green tests demonstrated missing structured windows, missing daemon prompt
context and a context overflow resolved by repacking. Tests cover start/end
clipping, exact boundary intersection, overlapping excerpt deduplication,
invalid offsets, chunk isolation, refusing oversized notes before a transport
call, and verifying a prompt cannot contain a second meeting's fixture text.
These are synthetic-provider tests; real model quality evaluation remains open.

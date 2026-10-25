# Notes editor progress (#122 / #13)

Saved meetings have an editable textarea with 500 ms debounce, a 5-second
maximum debounce, blur/navigation flush, and explicit save/retry. Each draft
owns its meeting ID. Navigation leaves unfinished writes associated with that
ID; edits made during a save are queued behind it using the returned revision.

Unacknowledged edits stay in tab memory and unloading warns about unsaved text.
Acknowledged recording-session drafts live in the encrypted library and appear
under Unfinished notes after a browser restart. No note text is written to browser
storage. Keystrokes not acknowledged before a crash can still be lost.

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
73 Node UI tests pass. Full workspace regression is also run before pushing.

Remaining acceptance includes trusted keyboard/IME, mobile selection, physical
recording/clock/device behavior and transcript-window grounding for augmentation.
The saved-meeting editor remaps supplied anchors without creating historic
timestamps. The live first-input implementation is described below; physical timing
acceptance remains part of #13.
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
73 UI tests pass. Trusted keyboard/IME, mobile selection and live recording-time
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

The store contract covers acknowledged drafts. The finalization and live adapters
described below supply capture/stop bounds, handoff and recovery discovery.
Unacknowledged keystrokes are not covered by database durability. No browser
storage is added; native acceptance and augmentation keep #122 and #13 open.

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
live-note routes and UI below consume these bounds for #122/#13.

## Live editor, authenticated clocks and recovery

The session-note API takes the active recorder clock or the finalized alias clock,
never a client-supplied recording duration. Unknown sessions are refused. An
unfinished durable draft whose recorder clock is gone can be read and exported,
and saves are refused until authoritative bounds are available. Saved aliases
continue to accept revision-checked late writes to the canonical note.

The live editor captures an anchor on first input for each new block, using a
monotonic interpolation of the recorder's latest elapsed time. Edits and movement
keep original identities/times; saved-meeting edits create no historic times.
Stop freezes the client clock and flushes pending text. The actual event-hub
handoff links the same draft to the saved meeting, retaining in-flight edits.
The Unfinished notes control lists durable drafts with pagination and retry;
recovered text is read-only and offers Markdown/JSON exports and copy/manual copy.

### Browser evidence

`cargo run -p fotwd --example ui_preview -- --live-notes` runs one synthetic
recording with the real encrypted finalizer and event hub. It opens no microphone
and reads no user keys. Rerun the example to reset its one-shot recording.

An isolated nonpersistent WebKit view verified:

- First-input anchors within 500ms of the sampled recorder clock.
- Autosave and recovery in a fresh view while the synthetic capture continued.
- Saved-history browsing and Return to recording without text/identity leakage.
- A session-note POST delayed 400ms across Stop, canonical event-hub navigation,
  preserved text and anchors, and frozen bounds in the saved meeting.
- Read-only recovery, refused writes without a clock, and Markdown/JSON export
  retaining Unicode text and anchor metadata. Download Blobs were inspected in
  memory; native Downloads and the system clipboard were not used.

| State | Synthetic screenshot |
| --- | --- |
| Recording, narrow viewport | [Live editor](live-notes/recording-390.png) |
| Saved after Stop and delayed write | [Canonical meeting](live-notes/saved-after-stop.png) |
| Read-only unfinished draft | [Recovery](live-notes/recovered-draft.png) |

All 73 Node UI tests and the authenticated session-note API regression pass.
Database reopen/migration/archive tests establish storage durability separately.
Trusted keyboard/IME, real capture-stop/drain/device/sleep scenarios and process
crash acceptance still need validation; augmentation window integration remains
#13 work. Neither #122 nor #13 is closed by this increment.

## Anchored augmentation context

Augmentation reads one meeting's note document and anchors together, then gives
each anchored block a recording-relative ±90-second window. Windows clip to the
recording bounds; out-of-recording anchors supply no evidence. Matching excerpts
are restricted to the current map chunk and deduplicated across overlapping
windows. Boundary-overlapping turns retain their original text and timestamps;
the code does not invent word-level clipping. Transcript segment IDs remain the
citation source. The prompt treats note/excerpt text as untrusted data and asks
the model to expand each block only from its matching excerpt IDs.

Structured note payloads reserve context before provider calls, with a conservative
byte-length ceiling. Chunks are repacked to fit or a context-overflow error is
returned. Notes without anchors receive the same preflight budget protection.
A new regression first reached the mock provider with an oversized unanchored
note; after the guard fix it rejects the input with zero provider calls.

The summarizer suite and daemon integration tests verify original offsets,
edge clipping, overlapping-window deduplication, map-chunk isolation, context
repacking/overflow and exclusion of another meeting's transcript. These tests
inspect actual mock-provider requests and use no real keys or cloud calls.
Model output quality, native timing and the remaining acceptance gates for
#13/#122 still require validation.


## Composition and editor ownership

Notes treat native marked text as a composition transaction. Input still updates
an in-memory draft, but the 500ms debounce/5s maximum autosave and an older save
loop do not publish intermediate candidates. Status updates never assign the
textarea value during composition. Movement, overwrite and discard controls are
disabled until composition ends; the status says Composing rather than Saved.
An uncommitted composition also triggers the unsaved-page warning.

The block mapping retains a composition checkpoint through an IME's transient
marked-range deletion, so final text keeps the first input's UUID/time. New
blocks can extend the checkpoint. Original unanchored text stays unanchored,
and a finalized capture clock bounds both the current anchors and the older
composition checkpoint. A late clock cannot restore a beyond-end timestamp.

Explicit Stop/navigation/blur snapshots the latest visible composition text to
its original draft identity. Untouched notes are not read back from the textarea,
so browser CRLF normalization cannot cause a save merely from navigation. Clearing a pane releases its editor, cancels timers, flushes its
draft and disconnects old callbacks. Late events from detached fields cannot
change a remounted live/saved alias or another meeting. These drafts remain in
memory; durable text still uses the existing revision-checked encrypted store.
Failed saves and conflicts retain the draft and require the existing explicit
retry/overwrite choices.

Behavioral tests reproduce preedit autosave, status-driven text replacement,
stale navigation, save-loop completion races and late finalized clocks. The
committed [WebKit trace](../crates/fotw-web/tests/fixtures/notes-ime-webkit.json) records an actual WebKit event
sequence: deleteCompositionText temporarily empties the marked range before
insertFromComposition supplies the final text. Node regressions replay it.

On macOS 26.4, an owned nonpersistent, nonactivating WKWebView used only its own
NSTextInputClient setMarkedText/insertText methods against
`ui_preview --live-notes` and its temporary encrypted database. At 390px, two
Japanese/emoji paragraphs each remained unsaved during a 5.5-second preedit,
then produced exactly one committed POST with their initial block timestamps.
At 1440px, Stop during a third composition preserved the visible text through
the real event-hub saved-meeting handoff; late events on the detached field
could not change saved notes or advance its revision. No global keyboard event,
clipboard, system input-source change, microphone, provider or user library was
used. This verifies the browser marked-text lifecycle; a physical keyboard/IME
locale matrix, actual capture/device behavior and crash acceptance remain open.

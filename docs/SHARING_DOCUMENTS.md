# Purpose-aware sharing documents

Open a finished meeting and use **Meeting document**. The configured summary
engine reads the transcript and notes to infer the meeting's purpose and audience.
For example, a creative call can produce a production brief, a client call a recap,
and a design meeting a decision record. Optional purpose and audience fields let
reviewers steer a new draft. New creative proposals are labeled as suggestions;
uncertain owners, dates, and decisions should remain open questions.

Automatic drafts are enabled by default for meeting enrichment, using the existing
approved summary engine. The checkbox in this panel turns that off globally.
There is no new provider or login. With no engine configured, no document is sent
for generation; an older meeting can be processed with **Create & download .md** after
configuring an engine. Existing drafts are never automatically replaced. This
adds one model call per eligible meeting, with one repair attempt for invalid output.
Both calls share a five-minute deadline. Failed
automatic attempts can be retried manually; there is no separate retry queue.

## Create and download

**Create & download .md** generates the draft, saves it in the meeting, and starts
one Markdown download automatically when generation succeeds. The file goes to
the browser's configured download folder (normally Downloads). A failed generation
never downloads an empty file. If a download cannot start, the saved draft stays
available and **Download .md** retries without another model call.

For a meeting with a saved draft, the main button is **Download .md**. After editing,
it becomes **Save & download .md**, saving a revision before downloading the edited
copy. Reopening a meeting or finishing automatic background generation does not
trigger unsolicited or duplicate downloads.

**Customize** contains optional purpose, audience, transcript inclusion, automatic
draft settings, and **Create new draft & download .md**. **Review or edit** contains
the Markdown editor, review notes, and transcript excerpt checkboxes. Uncheck
additional excerpts to leave them out; the original meeting remains intact.
**Remember name correction** stores a meeting-specific spelling correction and
updates the summary and saved brief. Regeneration keeps the correction. Source
names in browser sharing copies appear in brackets; the original transcript is
unchanged. See [attendee context and name repair](MEETING_CONTEXT.md).

**Print / PDF** opens the browser print dialog; choose **Save as PDF**. Only the
reviewed document prints; navigation, the full source transcript, and review-only
notes are excluded. PDF uses the browser print dialog, not an automatic PDF file
download. Browser print headers/footers can be disabled in that dialog.

Exports use elapsed `HH:MM:SS` and actual Eastern time (`America/New_York`, EST or
EDT according to the timestamp), calculated from capture start plus segment offset.
The full date is included so midnight and repeated DST hours remain unambiguous.
For mic-only meetings, excerpts are labeled **Call audio**, since that microphone
may have captured multiple people.

The model proposes an end boundary and ranges of unrelated/private conversation
to omit. Those indices are validated before exact source wording is copied. This
is a reviewable sharing edition, not a guarantee of perfect redaction or an edit
to the original recording. Omitted spans are explicitly marked. No document is emailed automatically.

## GitHub sync

When GitHub export is enabled, a push also writes the current summary to
`<meeting>.summary.md` and the latest saved document brief to
`<meeting>.document.md`, alongside the existing full transcript Markdown.
The document companion contains the brief only; selected transcript excerpts are
included in browser downloads when selected. Review notes, unsaved edits, and old
document revisions are never included in companion files.

In auto mode, saved document revisions and new summary versions are synchronized
on the next worker pass (normally within a minute). Existing eligible meetings
receive missing companion files. Files keep stable paths, so a later sync updates
the same file instead of adding another. GitHub export is an archive, so its
original transcript file still contains the full recording even when a sharing
draft omits private tangents.

### What each meeting shows

There is no per-meeting push button. The automatic pass owns every push it
covers, so a button on a meeting already in the repository had nothing to do and
invited a second commit of it. Each finished meeting shows exactly one sync state
instead:

- **Not synced to GitHub yet** — export is on, and this meeting has not been sent.
- **Synced to `owner/name`**, with the date and time the commit landed.
- **Changed since** that sync — its summary or saved brief is newer than the copy
  in the repository.
- **Did not sync**, with the reason `gh` reported, and the date of the last
  successful sync where there was one.

Each line also says who will act next, which is a different question from what
the state is. The automatic pass covers a meeting only in auto mode, and only if
the meeting started after auto was switched on or **sync every meeting in the
library** is on. A meeting it covers says that the next pass will sync it and
offers nothing to click. A meeting it does not cover — every meeting while the
mode is manual, and anything recorded before auto was switched on — says so and
offers **Sync now**, which is the only way one of those reaches the repository. A
failed meeting offers **Retry** either way, and says whether an automatic retry
is coming as well.

A meeting whose push failed is retried on its own, after five minutes, then
fifteen, then once an hour for as long as it keeps failing — as long as the
automatic pass covers it. Neither a daemon restart nor a button press is needed:
the wait is stored in the library, so a restart resumes it instead of losing both
the wait and the reason. `fotwd.log` records a meeting entering and leaving that
wait rather than repeating a line on every pass in between.

When `gh` itself is the problem — not installed, not logged in, the repository
gone or refused as public — no meeting is set aside for it, because one broken
login would otherwise park the whole library. The pass stops, writes the reason
to `fotwd.log`, and every meeting it still owes shows that reason in place of a
promise that the next pass will sync it. The first push that lands clears it.

One pass sends at most ten meetings, oldest first, and takes the remainder on
later passes; when it stops at that limit it writes how many of the owed meetings
it sent. Nothing is skipped and nothing is sent twice.

### Syncing meetings recorded before auto was switched on

By default auto mode sends only meetings that started after auto was enabled, so
turning it on never publishes an existing archive. **Sync every meeting in the
library**, in the GitHub export settings, removes that cutoff: every finished
meeting becomes owed, oldest first, ten a pass, until the library is in the
repository.

It is off by default, and a settings row saved before it existed reads as off, so
no upgrade turns it on. Publishing cannot be undone — a commit stays in the
repository's history even after the file is deleted — which is why it is a
separate switch rather than part of auto mode.

### Private repositories only, unless acknowledged

Every push, and every sync of the `index.md` and `log.md` index files that would
write to the repository, first asks GitHub about the repository and writes nothing
(error `repo_is_public`) unless the answer confirms it is private: `private` must be `true`, and `visibility`, when
present, must not be `public`. A missing `private` field or an answer that is not
JSON counts as public. The check runs on every write, not once when the settings
are saved, because a repository can be made public after it was configured, and
everything pushed to a public repository is readable by anyone and stays in its
history: transcripts and notes, summaries, briefs, and meeting titles in file names
and commit messages. The repository suggestions leave public repositories out.

The only exception is an acknowledgement stored with the GitHub export settings
(`allow_public_repo`). The dashboard offers it as **push to a public repository
anyway** in the GitHub export settings. Editing the repository name clears the
tick, so an acknowledgement given for one repository does not carry over to
another. In auto mode a refusal ends that pass and is written
to `fotwd.log`. No meeting is set aside as failed, so owed meetings are pushed on a
later pass once the repository is private or the acknowledgement is stored.

## Implementation and boundaries

- `fotwd::documents` shares the existing engine adapters, transport allowlist and
  CLI read shield. Provider error content is not exposed by document errors.
- `POST /api/meetings/{id}/document` supports `load`, `generate`, `save`, and
  `configure`, behind the same bearer/origin/host ingress guard as meetings.
- Manual generation is limited to one simultaneous request per daemon controller.
  It never holds the database mutex during model work. Optimistic revisions reject
  stale saves or a racing generation result. Enrichment independently refuses to
  replace a document another window created while its model ran.
- Migration 0004 stores append-only JSON snapshots in `meeting_documents`, with a
  meeting foreign key and cascade deletion. A snapshot contains the prose and
  selected source excerpts, so later source changes do not alter an existing draft.
  All revisions travel in JSON/library archives. The UI shows the newest revision;
  previous revisions are retained in the library/archive, not a history picker.
- Transcripts above 200 KB of text or 4,000 segments are refused explicitly; they
  are never silently truncated. Model JSON, source indices, ranges, factual-section
  evidence and output completeness are validated. Evidence-index validation does
  not prove every generated claim; the human review remains necessary.
- Drafts and unsaved edits remain available while navigating meetings in the same
  page. Save before refreshing or closing the tab. Interrupted generation can be
  retried; the last saved revision remains available.

## Verification

Rust tests cover request validation and authentication, intact source wording,
invalid and excluded evidence, truncated output, automatic-generation skips,
revision conflicts, cascade deletion, and lossless library archive round-trips.
`node --test crates/fotw-web/tests/ui/*.cjs` checks Eastern timestamps through DST,
omission markers, reviewer exclusions, and Markdown escaping for source excerpts.
It also drives every GitHub sync state through a DOM, asserting that no state
offers a push button, that a control appears only where no automatic pass will
act, and that switching meetings mid-load leaves one sync line rather than two.
Rust tests cover the whole-library switch defaulting to off, the ten-per-pass
limit taking the oldest first, a failed push becoming eligible again after its
wait and still reporting its reason after a restart, and the state agreeing with
the pass about which meetings it covers.
Browser QA exercises edit/selection/save and reviews a multi-page printed document,
including Unicode and exclusion of surrounding library content.

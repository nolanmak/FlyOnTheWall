# Attendee release

Every meeting is **confidential by default**. Attendee release is the opt-in
way to share one meeting with the people who were invited to it. FlyOnTheWall
emails them a reviewed copy: a short summary, the decisions, the action items
and a cleaned transcript. It sends only to invitees whose address is in a
domain you allow, and nothing goes out until an independent model review of
the exact email has run. In approval mode, the default, you must also approve
it.

Mail goes through the Google Workspace CLI (`gws`), using your own login, in
the same way GitHub export uses `gh`. There is no FlyOnTheWall server and no
mail relay.

The design record is issue [#326](https://github.com/nolanmak/FlyOnTheWall/issues/326).
This page describes what the code does.

## Archive and release

FlyOnTheWall has two ways to share a meeting, and they answer different
questions.

| | GitHub export (the **archive**) | Attendee release |
|---|---|---|
| Question | Keep everything for the owners. | Give each attendee what they may see. |
| Audience | Everyone with access to one repository. | The invitees of one meeting who are in an allowed domain. |
| Content | The full raw transcript, notes, summary and saved sharing brief. | A release summary and a cleaned transcript, prepared only for this purpose. |
| Undo | None. Git history keeps what was pushed. | None. A sent email is final. |
| Doc | [GitHub sync](SHARING_DOCUMENTS.md#github-sync) | This page |

The two are independent. Release never writes sharing-document revisions,
meeting summaries, or anything GitHub export reads. GitHub export output for a
meeting is the same whether release is on or off. "Release" here means
emailing a meeting. It has nothing to do with a software release, with
`export`, or with sync. Approval mode and auto mode are release modes and have
nothing to do with GitHub export's manual and auto.

## Off by default, and the cutoff

Release is off until you turn it on in **Settings → Attendee release**. To
turn it on you need at least one allowed domain, a `gws` login that passes the
preflight check (see [Setting up `gws`](#setting-up-gws)), and a summary
engine that the summary policy allows. If any of these is missing, the save is
refused with a message and nothing changes.

Each time release goes from off to on, the **server** sets the cutoff
`release_since_ms` to the current time. Any value the client sends is
ignored. Only meetings that **started at or after the cutoff** can be released.
Turning release on never emails a meeting recorded earlier, and neither does
turning it off and on again after a gap: that moves the cutoff forward. There
is no switch that releases the whole library.

Turning release off stops the worker from creating, preparing and sending.
Existing rows keep their state. If you turn it on again, every row that was
`scheduled` goes back to review with the reason `reenabled_while_scheduled`.

## Who gets the email

Recipients come only from the calendar and your domain list. A model never
chooses them, so a prompt injection in a transcript cannot add a recipient.

1. **One confidently matched event.** Release starts from the calendar event
   that the existing matcher already picked to name the meeting. It looks up
   that event again in the same calendar (Apple Calendar through EventKit, or
   Google through Composio) for the recording window. For a recurring event it
   uses the occurrence that overlaps the recording. If the event is gone, the
   reason is `event_not_found`. If the calendar can't be read in full, the
   reason is `calendar_unavailable` and the lookup is tried again later.
2. **The match must be confident.** The candidates are the events the matcher
   would consider: declined, untitled and long focus blocks are left out. Copies
   of one invite (the same iCalendar UID and start time, as seen in both Apple
   Calendar and Google) count as one event. The match is confident only when
   exactly one candidate has **at least half of its own scheduled length**
   inside the recording, and that candidate is the event the meeting was named
   after. A meeting that ran long still passes. A recording that covers less
   than half of its event never does. Otherwise the reason is
   `no_confident_match`, which is also the reason for a meeting with no matched
   event at all. Release never borrows attendees from nearby events.
3. **Attendees, minus exclusions.** The recipients are that event's attendees,
   including the organiser and you. Rooms and resources, groups, and anyone
   whose response is **declined** are left out. If the same address appears
   twice and either entry is excluded, the address is left out.
4. **Allowed domains only.** Addresses are lowercased. Only addresses whose
   domain **exactly** equals an allowed domain are kept. `sub.example.com` does
   not match `example.com`. Addresses outside the allowed domains are never
   stored. If no one is left, the reason is `no_recipients`.
5. **Frozen.** When the release row is created, the list is stored on it along
   with the event title, the calendar source, the event id and the iCalendar
   UID. Nothing updates the list after that. A later decline or a new invitee
   is ignored (see [R6](#accepted-risks-and-limits)). At send time the list can
   only **shrink**: it is filtered again against the current allowed domains.
   If you remove a domain, its addresses are not sent to. If nobody is left,
   the row ends as `dropped` with reason `no_recipients`.

A release with more than 50 recipients always needs review
(`many_recipients`). The list is not shortened.

For example, take an event with five attendees: you (`you@example.com`), a
colleague (`colleague@example.com`), a colleague who declined
(`declined@example.com`), a guest (`guest@example.org`) and a meeting room.
With `example.com` allowed, the release is addressed to exactly two people:
you and the colleague.

### Eligibility

The worker creates a release row only when all of these hold:

- release is on and the stored settings can be read;
- the meeting started at or after the cutoff;
- the meeting is `ready`, has a primary transcript, and its enrichment has
  finished (the same readiness check GitHub export waits for);
- the transcript has at most 4,000 segments and at most 200,000 bytes of text;
- the event match is confident and leaves at least one recipient;
- the meeting ended less than **7 days** ago.

The 7-day window only limits when rows can be created. A row that already
exists never expires.

## What the email contains

There is one email per meeting. Every recipient is in `To:`, with no Cc or Bcc.
It is sent from your verified sender address.

- **Subject:** `Notes: <event title> (<YYYY-MM-DD>)`. The title is the raw
  calendar title that a person wrote, not one derived from the transcript.
  The date is in the meeting's time zone. Control characters and newlines
  become spaces, and the subject is capped at 150 characters. A long title is
  shortened so the date stays.
- **Body:** plain text with a **Summary** section and, when they have
  bullets, **Decisions** and **Action items** sections. Each bullet cites
  transcript segments, but the citations are stripped from the email and kept
  on the row for the review and the Outbox. A body over 64 KB is held as
  `body_too_large`.
- **Attachment:** `<YYYY-MM-DD>-<slug>.md`. The slug is the event title
  lowercased, with every run of characters other than `a–z` and `0–9`
  replaced by `-`, trimmed, at most 60 characters, and `meeting` if nothing is
  left. The file contains a `# Notes: <title>` heading, a line with the
  meeting's date, time and zone, and then each retained segment as
  `**HH:MM:SS · <speaker>** <text>`. Name corrections appear in brackets. Each
  run of left-out segments becomes `*[… N segments omitted …]*`, and a run to
  the end becomes `*[… remainder omitted …]*`.

Nothing else is sent: no sharing brief, meeting summary, notes, audio,
findings or raw transcript. The transcript wording is copied by code from the
stored segments and is never rewritten by a model.

## How a release is prepared

Preparation makes three model calls and one render, all with the summary engine
you configured and the same transport allowlist as sharing documents. Each call
has a 5-minute deadline and one repair attempt for invalid output.

1. **Cut.** The model reads the whole transcript and returns only an
   `end_index` (where the meeting proper ends, judged from the content and never
   from a clock, since meetings often run late) and `omit_ranges` (off-topic or
   private segments). It returns no prose. The indices are checked by the same
   validator sharing documents use. The **retained set** is every segment up
   to `end_index`, minus the omitted ranges.
2. **Release summary.** A second call sees only the retained excerpts, the
   recipients' display names (to spell names correctly) and the meeting's name
   corrections. It never sees notes, the meeting title, the meeting summary or
   anything that was cut. Every bullet must cite at least one retained segment.
3. **Render.** Code builds the exact subject, body and attachment, plus an
   index map that links attachment lines and body bullets back to their
   segments.
4. **Review.** A third call, with a fresh context and its own prompt, reads the
   exact rendered subject, body and attachment, the index map and the
   recipients. It asks one question: may these specific people read this exact
   email? It reports findings in six categories:

   | Category | What it covers |
   |---|---|
   | `off_meeting` | Personal or non-work talk, and anything after the meeting wrapped up |
   | `about_recipient` | The performance, pay, employment or health of anyone on the recipient list |
   | `compensation` | Pay rates, salaries, bonuses, equity, commissions, margins |
   | `legal_financial` | Legal advice, litigation, entity structure, tax, banking details |
   | `credential` | Passwords, door codes, one-time codes, API keys, tokens |
   | `offensive` | Slurs and demeaning remarks |

   A finding names the category, the retained segments and/or the body bullet
   it concerns, and a reason of at most 200 characters. The review treats
   everything in the packet as data and never as instructions.

What the review approves is exactly what gets sent. The Outbox always shows the
**stored** subject, body and attachment, never a fresh render.

If a call fails, times out, or returns invalid output twice, the row is held as
`model_failed`.

## Approval mode and auto mode

**Approval** is the default. Every prepared release waits in the Outbox with
the reason `review_required` until a person approves or drops it.

**Auto** schedules a prepared release to send after the send delay, with no
person involved, but only when all of these hold:

- the review returned **zero findings**;
- there are no more than 50 recipients (`many_recipients`), and at least 30% of
  the meeting's segments were kept (`mostly_omitted`);
- the release did not come from **Edit cuts** (`edited`);
- **every recipient** has already been sent a release from this machine.
  First-time recipients always need review (`first_time_recipient`);
- auto is unlocked for the current engine id (otherwise `auto_locked`).

Otherwise the row goes to `needs_review` with every reason that applies. The
send delay, `send_delay_minutes`, defaults to 15 and can be 0 to 120. It is
fixed into the row's send time when the row is scheduled. With a delay of 0 a
release sends as soon as it is scheduled, and there is no chance to cancel it.
The settings panel warns about this.

### How auto unlocks

The settings save refuses `mode = auto` until there is evidence from your own
decisions:

- at least **20 human decisions** for the current **engine id**, and
- at least **80%** of them **approved unchanged**.

Human decisions are *approved unchanged* (Approve with no Edit cuts),
*approved edited* (Approve after Edit cuts) and *dropped* (Drop). Auto-scheduled
sends and **Send again anyway** are recorded but do not count. Cancelling an
auto-scheduled send records nothing. The decisions are kept in an append-only
table that is not deleted along with meetings, so deleting meetings can't
change the unlock.

The engine id is the engine kind plus a model name:

- `anthropic-api:<model>` for the Anthropic API engine. The model is the one
  the sharing-document path asks for (currently `claude-opus-5`).
- `claude-cli:default` and `codex-cli:default` for the CLI engines. The CLIs
  run their own default model, which FlyOnTheWall can't see, so the id says
  `default` rather than naming a model.

Switching to a different engine starts the count again for that engine id.
Changing a CLI's default model does **not** change the engine id, so it does
**not** re-lock auto mode. If you change the model your `claude` or `codex`
CLI uses, consider switching back to approval mode until you trust the new
model.

The rule is checked again on every worker pass. If it stops holding, for
example because you switched engines, auto acts like approval: new rows get
`auto_locked`, and `scheduled` rows that auto approved go back to review with
`mode_changed`. Switching from auto to approval in Settings does the same to
auto-approved `scheduled` rows. Switching from approval to auto doesn't touch
existing rows. The mode is read when a row finishes preparing.

## The five kinds of "not sent"

A meeting that was not emailed is in exactly one of five situations. Each one
shows a reason code. The dashboard shows each code as words.

### 1. Not eligible (no row)

No release exists. The meeting page shows `Not released: <reason>`. While
release is off the line is hidden, because every meeting would say the same
thing. The worker caches the reason per meeting and checks again at most every
15 minutes inside the 7-day window.

| Code | Meaning |
|---|---|
| `release_off` | Release is off. |
| `before_cutoff` | The meeting started before release was turned on. |
| `not_ready` | Not finished, not summarised, no transcript, or not yet evaluated. |
| `no_confident_match` | No single event clearly is this recording, or none was matched. |
| `no_recipients` | No invitee is in an allowed domain. |
| `too_long` | More than 4,000 segments or 200,000 bytes of transcript text. |
| `window_passed` | More than 7 days since the meeting ended. |
| `event_not_found` | The matched event is no longer in the calendar. |
| `calendar_unavailable` | The calendar could not be read in full; tried again later. |

### 2. Held (row; Retry or Drop)

Something failed while preparing or sending, and no email went out.

| Code | Meaning | Actions |
|---|---|---|
| `model_failed` | A model call failed, timed out, or gave invalid output twice. | Retry (prepares again), Drop |
| `body_too_large` | The email body would be over 64 KB. | Retry, Edit cuts, Drop |
| `send_refused` | Google rejected the send (an HTTP 4xx answer). Nothing was sent. | Retry (resends the same snapshot now), Drop |

### 3. Needs review (row)

The release is waiting for a person. A row stores **every** reason that applies,
and the Outbox shows all of them.

| Code | Meaning |
|---|---|
| `review_required` | Approval mode, or a scheduled send was cancelled. |
| `has_findings` | The review flagged something. |
| `first_time_recipient` | At least one recipient has never been sent a release. |
| `many_recipients` | More than 50 recipients. |
| `mostly_omitted` | Fewer than 30% of the meeting's segments were kept. |
| `edited` | You edited the cuts. |
| `reenabled_while_scheduled` | Release was turned off and on while this was scheduled. |
| `delayed_while_paused` | Sending was paused for more than 60 minutes after this was due. |
| `mode_changed` | Auto mode was switched off or locked while this was scheduled. |
| `auto_locked` | Auto mode is selected but not unlocked for this engine. |
| `inputs_changed` | The transcript or name corrections changed after preparation. |
| `send_outcome_unknown` | A send started but its result is unknown. It may already have been sent. |

`inputs_changed` works from a fingerprint. Each row stores a SHA-256 hash of
the primary transcript's segments and the meeting's name corrections. Notes
and the meeting title are not inputs, so editing them changes nothing. Rows in
`needs_review`, `scheduled` or `held` are checked on every pass, and Approve
and Retry check too. If the hash differs, the release is prepared again from
the cut and ends in `needs_review(inputs_changed)`. The exception is a
`send_outcome_unknown` row, which stays where it is.

### 4. Worker paused (no row changes)

Something the whole worker needs is missing. Rows keep their state. A banner
appears in Settings and in the Outbox. The check runs again on every pass, and
the worker resumes by itself once the problem is fixed.

| Code | Blocks | Cause |
|---|---|---|
| `gws_unavailable` | Sending only | No verified sender is stored, or the preflight failed: `gws` would not start, `getProfile` failed, the account is not the verified sender, or the sender's domain is no longer allowed. A send whose `gws` process could not start also pauses. |
| `engine_unavailable` | Preparing only | The summary policy refuses, or no summary engine is configured. |
| `settings_unreadable` | Everything | The stored release settings exist but cannot be read or decoded. |

Missing settings mean the defaults, which is release off. A **damaged** value
never falls back to the defaults. Saving a complete replacement in Settings
fixes it. A `scheduled` row whose send time passed more than 60 minutes ago
while the worker was paused goes to `needs_review(delayed_while_paused)` and is
not sent.

### 5. Terminal

- `sent`: the email went out. The row records the addresses actually used and
  the message id when `gws` returned one. Nothing ever sends a second email for
  this meeting automatically.
- `dropped` with reason `by_user` (you dropped it) or `no_recipients` (every
  recipient was filtered out at send time).

## Sending at most once

A send moves the row from `scheduled` to `sending` and commits that change
**before** `gws` starts. `gws` runs once, in its own process group, with a
120-second limit. On timeout the whole group is killed. The result is then
read:

- **Exit 0:** `sent`.
- **The process could not start at all** (no binary, or the OS refused): the
  row goes back to `scheduled` and the worker pauses as `gws_unavailable`. This
  is the only automatic retry, and it is safe because no process ran. A
  process that starts and then fails for any reason, including an npm install
  that can't find `node`, does not count as "could not start".
- **Exit 1 with an HTTP 4xx error from Google:** `held(send_refused)`.
- **Anything else** (a timeout, a 5xx, an unrecognised error, or a daemon that
  stopped while the row was `sending`): `needs_review(send_outcome_unknown)`.

At startup, every row left in `sending` moves to
`needs_review(send_outcome_unknown)`. A duplicate email is recoverable. A loop
that automatically resends confidential content is not.

## The Outbox

The **Release outbox** lists every row in `needs_review`, `scheduled` or
`held`, oldest first. The badge counts `needs_review` plus `held`. Opening a
row shows the frozen recipients (name and address), every reason, each finding
linked to its excerpt or bullet, the stored subject, body and attachment, and
every transcript segment with a tick for the ones that will be sent.

| Action | Available on | What it does |
|---|---|---|
| **Approve & send** | `needs_review` (not `send_outcome_unknown`) | Schedules the send for now. Records *approved unchanged*, or *approved edited* after Edit cuts. If the inputs changed, the release is prepared again instead. |
| **Edit cuts** | `needs_review` (not `send_outcome_unknown`); `held(body_too_large)` | Tick any segments, including ones the model left out. Saving reruns the summary, render and review from your selection. The result always returns to `needs_review(edited)`, even in auto mode. |
| **Drop** | `needs_review`, `held` | Ends as `dropped(by_user)` and records *dropped*. |
| **Cancel send** | `scheduled` | Back to `needs_review(review_required)`. It only works if it lands before the send starts. Records nothing. |
| **Retry** | `held` | `model_failed` and `body_too_large` are prepared again. `send_refused` resends the same snapshot now. |
| **Send again anyway** | `needs_review(send_outcome_unknown)` only | Asks you to confirm first, because the email may already have gone out. Check the sending account's Sent folder before confirming. Records *sent again*, which does not count toward the auto unlock. |

Every action is a compare-and-set on the row's current state. If the row has
moved on, for example because a send started before your Cancel arrived, the
action changes nothing and the Outbox asks you to reload.

### The meeting line

Each meeting page shows one release line:

| State | Line |
|---|---|
| not eligible | `Not released: <reason>` |
| `preparing` | `Preparing release` |
| `needs_review` | `Waiting for your review` (`send_outcome_unknown`: `May already have been sent; check before resending`) |
| `scheduled` | `Sending to N people at HH:MM` |
| `sending` | `Sending` |
| `held` | `Held: <reason>` |
| `sent` | `Sent to N people, <date time>` |
| `dropped` | `Not released (dropped)` |

### Owner notice

If you set a **Notify address** in an allowed domain, FlyOnTheWall emails it
when releases are waiting. The subject is
`FlyOnTheWall: N meetings waiting for release review` and the body is empty.
A notice goes out only when a row has entered `needs_review` or `held` since
the last notice, and at most once an hour. The time of the last notice is
recorded only after a successful send, so a failed notice is tried again on a
later pass.

## Setting up `gws`

Release sends mail only through the
[Google Workspace CLI](https://github.com/googleworkspace/cli) (`gws`),
verified against version 0.22.5.

1. **Install `gws`.** Any install works. The daemon looks for it the same way
   it looks for `gh`: on its `PATH` first, then `/opt/homebrew/bin/gws`, then
   `/usr/local/bin/gws`.
2. **Make a profile folder for the sending account.** `gws` keeps its
   credentials in the folder named by `GOOGLE_WORKSPACE_CLI_CONFIG_DIR`. Give
   release its own folder and log in with the account that should send:

   ```sh
   export GOOGLE_WORKSPACE_CLI_CONFIG_DIR="$HOME/.config/gws-release"
   gws auth login -s gmail
   gws gmail users getProfile --params '{"userId":"me"}' --format json
   ```

   The last command is the preflight that FlyOnTheWall runs. Its
   `emailAddress` must be in one of your allowed domains. If you created the
   profile with `GOOGLE_WORKSPACE_CLI_KEYRING_BACKEND` set, the daemon needs
   that variable in its environment too. It is the only `GOOGLE_WORKSPACE_*`
   variable passed through to `gws`. All the others are removed, so a token or
   log setting in the daemon's environment can't change the sending account.
3. **Fill in Settings → Attendee release:** the allowed domains, the **gws
   profile folder** (the folder from step 2), and optionally the **gws
   binary**.
4. **Save with release turned on.** The save runs the preflight. When it
   passes, the profile's address is stored as the **verified sender** and the
   panel shows `Sends as <address> (verified).`

### `gws_binary` for npm installs

An npm install of `gws` usually lives somewhere the daemon's `PATH` can't see,
such as a Node version manager's folder. The `gws` command it installs is also
a `#!/usr/bin/env node` script, so it needs `node` on the daemon's `PATH`. If
`node` is missing, the script starts and exits non-zero. The preflight then
fails with `gws_unavailable`, and a send in progress becomes
`send_outcome_unknown`, never a silent retry.

Set **gws binary** to an absolute path. The simplest choice is the native
binary that the npm package ships, which needs no `node`:

```sh
echo "$(npm root -g)/@googleworkspace/cli/bin/gws"
```

When `gws_binary` is set, only that file is used. The daemon never falls back
to another `gws` on `PATH`.

### The verified sender and `--from`

Every send runs:

```
gws gmail +send --from=<verified sender> --to=<a@example.com,b@example.com> \
    --subject=<subject> --body=<body> -a <attachment> --format json
```

The notice has no `-a`. `gws` is given an argument list, never a shell string.
Every free-text value uses the `--flag=value` form, so a subject or body that
starts with `-` can't be read as a flag. The attachment is a temporary file at
`<data folder>/release-tmp/<meeting id>/<YYYY-MM-DD>-<slug>.md`. The folder is
mode 0700 and the file is mode 0600. The file is removed after the call, and
the whole `release-tmp` folder is removed when the daemon starts.

Before the first send of a tick, and on every pass while paused, the send loop
runs the preflight again. It caches a pass for 10 minutes. The account's
address must still **equal** the stored verified sender and still be in an
allowed domain. If you log the profile into a different account, sending
pauses with `gws_unavailable`. To verify a new account, save the settings with
a changed profile folder, binary or domain list. That triggers a fresh
preflight. Turning release off and on also re-verifies, but it moves the
cutoff.

The profile folder alone does not prove which account sends. During testing,
a fresh empty folder still reached Google with *a* credential. The `getProfile`
check and the sender equality are what pin the account.

## Privacy

- **No content in logs.** `fotwd.log`, API errors and the owner notice contain
  only meeting ids, state names and reason codes. They never contain addresses,
  titles, transcript or summary text, or `gws` output. `gws` stderr is read
  only to classify the failure and is then discarded. This is stricter than
  GitHub export, which logs the first line of `gh` stderr.
- **Device-local settings and tables.** Release settings live in the library's
  `app_meta` table under `release_settings`, and the last-notice time under
  `release_notice_last_ms`. The release rows (`meeting_releases`), the
  eligibility cache (`release_eligibility`), the decisions
  (`release_decisions`) and the sent-address ledger (`release_sent_addresses`)
  are tables in the encrypted library. All of it applies to this machine only.
- **Not archived, not synced.** `fotwd export-all` and `fotwd import` skip every
  `app_meta` key that starts with `release_`, and the release tables are never
  archived. Importing an archive can never turn release on, and no release row,
  decision or ledger entry travels with it. GitHub export never reads any of it.
- **What is stored about attendees.** Only while release is on, and only for a
  meeting that gets a release row: the matched event's title, its ids, and the
  invitees in your allowed domains (address, display name, response, and
  whether it is you). Addresses outside the allowed domains are never stored.
  The row is deleted with its meeting. The ledger keeps each address a release
  was **sent** to, with the time it first received one, **even after the
  meeting is deleted**. That is what makes the first-time-recipient check hold.
- **What goes to the summary engine.** The same engine and destination as
  summaries and sharing documents. The cut call gets the whole transcript. The
  summary call gets only the retained excerpts, the recipients' display names
  and the name corrections. The review gets the exact email (subject, body and
  attachment), the index map, and for each recipient the display name, first
  name and the part of the address before `@`. Full recipient addresses never
  reach the engine.
- **What passes through Composio.** If Google Calendar is connected through
  Composio, event titles, times and attendee lists pass through it when a
  recording is saved. Audio, transcripts and summaries never do. Attendee
  addresses are stored on this device the same way whether the event came
  from Google or from Apple Calendar.
- **What goes to Google.** The email itself, through your `gws` login, straight
  from this machine to the Gmail API.

## Accepted risks and limits

- **R1. The review can miss something.** It is a model, and so are the cut and
  the summary. The controls are that approval is the default, auto unlocks
  only from your own decisions, auto sends only with zero findings and known
  recipients, there is a send delay, and there is an eval gate. The unlock
  thresholds (20 decisions, 80%) will be revisited once there are 50 human
  decisions.
- **R2. Invited is not the same as present.** An invitee who left early or
  never joined still receives the whole retained meeting. The
  `about_recipient` category covers every recipient, whenever they were there.
- **R3. Composio may be a calendar source.** If Google Calendar is connected
  through Composio, Composio already sees calendar metadata, including the
  attendee list. Release adds no meeting content to that path.
- **R4. Other processes you run can read the `gws` command line.** While `gws`
  runs, a process running as the same user can read its arguments, which
  include the recipients, subject and body. `gws` has no flag to read the body
  from a file. The transcript attachment is a mode 0600 file and is not on the
  command line. This risk ends if `gws` adds such a flag.
- **R5. Google distribution lists look like people.** A group address on an
  invite can look like a single person. The first-time-recipient check and the
  recipient list shown in the Outbox are the controls.
- **R6. Calendar changes after a row exists are ignored.** Someone who declines
  late still receives the email. In approval mode you see the frozen list
  before approving, and the send-time domain filter still applies.
- **N7. Redaction is not guaranteed.** Release is a reviewed edition, not a
  guarantee that nothing private remains. Read the Outbox copy before you
  approve.

Release also deliberately does not do these things:

- send corrections or a second email;
- send to anyone outside the allowed domains, even manually;
- release meetings from before the cutoff;
- send by any path other than `gws`;
- send different content to different recipients, or remove one recipient in
  the Outbox;
- add recipients by any other route, such as manual entry or a backfill when a
  domain is added later;
- run when the summary policy refuses sharing documents.

## Eval results

The review is measured on a synthetic corpus in
[`tools/release-eval/`](../tools/release-eval/README.md): 30 meetings with 37
planted passages (at least 6 per category) and 12 clean meetings, 7 of them
with hard negatives. Offensive passages use a `[SLUR]` placeholder. Checks
against real language use a private set that is never committed. Only its
numbers would be recorded. A planted passage counts as **caught** if its text
is missing from the final email or a finding covers it. A clean meeting is a
**false hold** if it gets any finding or is held.

The gate is: at least 95% caught overall, 100% for `credential` and
`about_recipient`, at most 20% false holds, and at least 90% retention.

Results, recorded in [#347](https://github.com/nolanmak/FlyOnTheWall/pull/347):

| Engine (eval runner label) | Daemon engine id | Caught | `credential` | `about_recipient` | Other categories | False holds | Retention |
|---|---|---|---|---|---|---|---|
| `claude-cli` (Claude Code 2.1.332, subscription default model), run 1 | `claude-cli:default` | 37/37 | 7/7 | 6/6 | 6/6 each | 0/12 | 683/683\* |
| `claude-cli`, run 2 | `claude-cli:default` | 37/37 | 7/7 | 6/6 | 6/6 each | 0/12 | 683/683\* |
| `codex` (codex-cli 0.153.4, configured default model) | `codex-cli:default` | 37/37 | 7/7 | 6/6 | 6/6 each | 0/12 | 683/683\* |

No run was held as `model_failed`. The `anthropic-api` engine has not been
measured.

This table will be updated when the eval is re-run through the real cut and
summary.

\* **These runs measure the review on its own.** The eval runner still uses a
temporary `FixturePreparer` in place of the real cut and summary. It keeps
every segment and writes one fixed summary bullet, so every planted passage
reaches the email and retention is 100% by construction. The gate is not met
until the eval runs again through the real cut and summary. The corpus is
generated and its planted passages are fairly explicit, so a perfect score
shows that the contract and prompt work end to end. It does not show that the
review is robust to subtle cases.

To run it yourself (it uses a real engine, so it never runs in CI):

```sh
FOTW_RELEASE_EVAL=1 cargo run -p fotwd --example release_eval -- \
    --engine claude-cli --jobs 4 --out /tmp/release-eval.json
```

## Reference

### Settings

Stored as JSON in `app_meta` under `release_settings`. Every field has a
default.

| Field | Default | Rule |
|---|---|---|
| `enabled` | `false` | Turning it on needs a valid domain list, a passing preflight and an allowed engine. |
| `mode` | `approval` | `auto` is refused until unlocked. |
| `allowed_domains` | `[]` | Lowercase ASCII, at least one dot, no `@`, no `*`, no empty label. Give an international domain in punycode. |
| `gws_profile_dir` | empty | Passed to `gws` as `GOOGLE_WORKSPACE_CLI_CONFIG_DIR`. |
| `gws_binary` | empty | Empty means it is looked up like `gh`. |
| `send_delay_minutes` | `15` | 0–120. |
| `notify_address` | empty | A lowercase address in an allowed domain. |
| `release_since_ms` | `0` | Set by the server only. |
| `verified_sender` | empty | Set by the server only, from a passing preflight. |

Saves that change `gws_profile_dir`, `gws_binary` or `allowed_domains` while
release is on run the preflight again. A save that changes them while release
is off clears the verified sender.

### HTTP routes

These routes sit behind the same ingress guard as the rest of the dashboard
API.

- `GET/POST /api/settings/release` returns `{settings, error?, auto_unlocked, paused?}`.
  A refused save returns the stored settings unchanged, plus `error`.
- `GET /api/meetings/{id}/release` returns `{state, reasons, recipient_count, send_at?, sent_at?}`.
- `GET /api/release/outbox` returns `{items, badge, paused}`.
- `GET /api/release/outbox/{meeting_id}` returns the stored snapshot.
- `POST /api/release/outbox/{meeting_id}` takes `{"action": "approve" | "send_again" | "drop" | "cancel" | "retry"}`
  or `{"action": "edit_cuts", "retained": [...]}`.

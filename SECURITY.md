# Security policy

FlyOnTheWall records meetings, keeps their transcripts in a local library, and
serves a dashboard on `127.0.0.1`. A flaw in it can expose conversations of
people who never chose to use it, so please report problems privately.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting:

**<https://github.com/nolanmak/FlyOnTheWall/security/advisories/new>**

Only you and the maintainers can see a report filed there. **Do not open a public
issue, pull request or discussion for a vulnerability**, and do not describe it
in an existing one.

A useful report includes:

- the commit you tested (`git rev-parse HEAD`), your macOS version, and how the
  app was started (`just run`, double-clicking the app, or a `fotwd` subcommand);
- the steps to reproduce, and what an attacker gains;
- for anything involving the dashboard, the browser you used. The 2026-08 design
  review found that Safari does not stop public web pages from reaching
  `127.0.0.1` ([docs/REQUIREMENTS.md §10.1](docs/REQUIREMENTS.md#10-security--privacy)),
  so test there: a request Chrome blocks may still get through in Safari.

**Never include real meeting data or secrets, even in a private report:** no
recordings, transcripts, meeting titles, attendee names, API keys, Recovery Keys,
`db.sqlite3` or `db.sqlite3.recovery` files, and no unredacted `fotwd.log` or
`audit.jsonl` content. Reproduce with a throwaway library and synthetic audio or
text, and redact any log line before you include it.

If the reporting form is unavailable, open a public issue that asks for a
private way to get in touch and contains no details of the problem.

## What to expect

FlyOnTheWall has a single maintainer working on it in their own time, so
handling is best effort. There is no guaranteed response time, no support
contract and no bug bounty. What you can expect:

- the report is read, and acknowledged in the advisory, as soon as the
  maintainer gets to it;
- questions and progress stay in the private advisory;
- a confirmed problem is fixed on `main`, and the advisory is published once
  the fix is there, crediting you unless you would rather not be named;
- if the maintainer does not think something is a vulnerability, you get the
  reasoning.

Please allow reasonable time for a fix before disclosing publicly. If a report
has had no reply for a while, a follow-up comment on the advisory is welcome.

## Supported versions

There are no releases yet. Only the current `main` branch is supported, and
fixes land there. Please check that a problem still reproduces on current `main`
before reporting it.

## Scope

The attack surface below is the one described in
[docs/REQUIREMENTS.md §10](docs/REQUIREMENTS.md#10-security--privacy) and
implemented in the files named. Reports about any of it are in scope.

### The loopback server

`fotwd serve`, which is also what double-clicking the app runs, listens on
`127.0.0.1`, port 8737 by default. Any web page the user visits can try to reach
it, so it carries twelve ingress controls, ING-01 to ING-12, each mapped to its
code in [crates/fotw-web/src/lib.rs](crates/fotw-web/src/lib.rs):

- a bind to the literal loopback address, with a check on the peer address;
- an exact allowlist on the raw `Host` header, against DNS rebinding, and on
  `Origin` when present, including on the WebSocket upgrade;
- a 256-bit bearer secret minted at each start, a single-use WebSocket ticket
  valid for at most 10 seconds, and a one-time launch handoff token valid for at
  most 30 seconds;
- no cookies;
- a bare 404 for every refused request, so a scanning page cannot tell that the
  app is running;
- a strict Content-Security-Policy on the dashboard;
- a state file holding the secret, mode `0600` inside a `0700` directory.

In-scope examples: a web page or DNS-rebinding setup that can read the library or
a live transcript, change settings, or start a recording; a way for a page to
tell that FlyOnTheWall is running; script injection through text that reaches the
dashboard, such as a transcript or a meeting title; another macOS user account
reading the port and secret.

### The library, the keychain and the Recovery Key

- The meeting library, `db.sqlite3`, is SQLCipher-encrypted with a 32-byte key
  kept in the macOS keychain (service `com.flyonthewall.fotw`, account
  `db:masterkey`). Provider API keys are kept in the keychain too.
- The Recovery Key (`fotw1-` followed by eight groups) derives, through
  Argon2id, a key that unseals a copy of the library key stored in
  `db.sqlite3.recovery`. The first-run ceremony and `fotwd recover` are in
  [crates/fotwd/src/recovery.rs](crates/fotwd/src/recovery.rs); the cryptography
  is in [crates/fotw-secrets/src/recovery.rs](crates/fotw-secrets/src/recovery.rs).

In-scope examples: the library key, an API key or a Recovery Key written to a
file, a log, the database or a process's arguments; opening the library without
the key; getting the library key out of `db.sqlite3.recovery` faster than
guessing the Recovery Key through Argon2id; a first run that creates a library
whose key was never shown to the user; transcript text, note text, meeting titles
or attendee names ending up in `fotwd.log` or `audit.jsonl`, which the design
forbids.

### GitHub export

A finished meeting can be pushed to a repository the user configures. The push
runs through the user's own authenticated `gh` CLI, so FlyOnTheWall never holds
a GitHub token, and every push is recorded in `audit.jsonl`. Code:
[crates/fotwd/src/github.rs](crates/fotwd/src/github.rs) and
[crates/fotw-web/src/github.rs](crates/fotw-web/src/github.rs).

In-scope examples: meeting content pushed to a repository or path other than the
configured one, pushed automatically while the export is set to manual, or pushed
without an audit line; text an attacker controls, such as a meeting title,
changing what `gh` is asked to do.

### Egress to transcription and summary providers

- Speech-to-text is Deepgram only. `fotw-stt` streams audio over a WebSocket to
  `api.deepgram.com` ([crates/fotw-stt/src/deepgram_wire.rs](crates/fotw-stt/src/deepgram_wire.rs)).
- Titles, summaries and meeting documents use an Anthropic API key when one is
  set. Without one, they use the `claude` or `codex` CLI as a child process, and
  only after the user has enabled it and acknowledged that transcripts leave the
  machine (`fotwd engine`). With neither, nothing is sent.
- The daemon's HTTP requests to providers go through an allowlist of hosts
  ([crates/fotwd/src/transport.rs](crates/fotwd/src/transport.rs)).

In-scope examples: a request reaching a host that is not on the allowlist, or
meeting content sent to a provider the user has not configured; a transcript
sent to a CLI engine that has not been acknowledged; meeting content, such as
something a participant says, that makes a CLI engine read files, run commands
or make network requests beyond answering the prompt.

### Also in scope

- `fotwd mcp`, the stdio MCP server, offers read-only tools over the library. A
  way to write to the library through it, or to reach data outside the library,
  is in scope. What an agent then does with meeting text it retrieved is up to
  that agent.
- `fotwd import` reads an archive file. A crafted archive that writes outside the
  data directory, or corrupts the library, is in scope.

## Known limitations, not vulnerabilities

These are documented gaps or deliberate boundaries of the threat model. There is
no need to report them. A pull request that closes the first one is welcome.

- **Recorded audio is not encrypted at rest yet.** While a meeting records, and
  until it is archived, its audio is in `sessions/<session>/system.pcm` and
  `mic.pcm`. After archiving it is in
  `media/<yyyy>/<mm>/<meeting_id>/system.opus` and `mic.opus`. Both locations
  are plain files under `~/Library/Application Support/com.flyonthewall.fotw/`.
  Only the library database is encrypted. See correction 6 in
  docs/REQUIREMENTS.md §9.5.
- **Anyone with access to your unlocked macOS user account.** Malware running as
  your user, or a person at your unlocked Mac, can read the audio files, the
  sealed recovery file and the state file holding the dashboard secret, and can
  use the dashboard. Same-user local malware is explicitly outside the threat
  model (docs/REQUIREMENTS.md §10.1).
- **The sealed recovery file travels with the database.** Anyone holding a copy
  of your disk or a backup holds `db.sqlite3.recovery`. What protects it is the
  Recovery Key and the cost of Argon2id. A weakness in that protection is in
  scope; the file being there is not.
- **What has left the machine cannot be deleted from it.** Audio sent to
  Deepgram, transcripts sent to Anthropic or a CLI engine, and anything pushed
  to GitHub are subject to those services' own retention. Deleting a meeting in
  FlyOnTheWall does not reach them.
- **Development builds and macOS permissions.** An unsigned or ad-hoc-signed
  build run from a terminal can inherit the terminal's audio permission. That is
  a development hazard documented in [CONTRIBUTING.md](CONTRIBUTING.md), not a
  vulnerability in the app.

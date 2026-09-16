# Changelog

Notable changes to FlyOnTheWall are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

There are no releases and no version tags yet. The `0.1.0` in `Cargo.toml` is a
placeholder, not a release, and how versions will be numbered is decided at the
first one.

## [Unreleased]

Everything on `main` today. It is an early build: macOS 14.4 or later only,
built from source.

**Limits worth knowing before you use it.** Recorded audio is not encrypted at
rest yet; only the library database is. Deepgram is the only speech-to-text
provider. Audio capture is implemented for macOS only: `fotw-audio` has Windows
and Linux modules behind its platform seam, and CI checks that it compiles for
Windows, but neither captures anything.

### Added

#### Capture

- System audio through Core Audio process taps, and the microphone, captured as
  two separate streams anchored to one host clock.
- A watchdog that rebuilds taps that stop delivering audio, and recovery when
  the audio device changes.
- A speaker-echo gate on the microphone feed (CAP-11), and cross-leg transcript
  dedupe (`fotwd dedupe`), so remote speech played through speakers is not
  transcribed twice.
- A write-ahead session directory per recording, so a crash does not take the
  recording with it.
- Recordings started from the dashboard stop automatically after two hours,
  with a countdown ([docs/RECORDING_GUARDRAILS.md](docs/RECORDING_GUARDRAILS.md)).

#### Transcription

- Deepgram streaming speech-to-text for both legs, with reconnect and gapless
  replay, 48 kHz to 16 kHz resampling, and a live transcript in the dashboard.

#### Library and keys

- A SQLCipher-encrypted meeting library with FTS5 full-text search and
  cascade delete.
- API keys and the library key stored only in the macOS keychain
  (`fotwd key`).
- A mandatory Recovery Key shown at first run, with `fotwd recover` and
  `fotwd recover --check`.
- Opus archiving of finished sessions into `media/`, and an audio retention
  engine with a disk budget (`fotwd retention`).

#### Summaries and documents

- A title, summary and action items when a meeting ends, produced with an
  Anthropic API key or, once the user has enabled it and acknowledged that the
  transcript leaves the machine, the `claude` or `codex` CLI (`fotwd engine`).
  Summaries are checked against the transcript for citations and evidence, and
  the dashboard says when a meeting has no summary or a weakly grounded one.
- Summary templates stored as files (`fotwd templates`).
- Purpose-aware meeting documents with Markdown download and print to PDF
  ([docs/SHARING_DOCUMENTS.md](docs/SHARING_DOCUMENTS.md)), and per-meeting name
  corrections ([docs/MEETING_CONTEXT.md](docs/MEETING_CONTEXT.md)).
- Summary requests over HTTP go through an egress allowlist of provider hosts.

#### Consent

- A consent engine with jurisdiction warnings and a disclosure kit
  (`fotwd disclose`). A warning that is blocking must be acknowledged before a
  recording starts. See [docs/CONSENT.md](docs/CONSENT.md).
- Meeting detection that only offers to record and never starts a recording on
  its own (`fotwd detect`).
- A local, append-only audit log (`audit.jsonl`) of recording starts and ends,
  declined detection prompts, and GitHub pushes.

#### Interfaces

- `fotwd serve`: the dashboard on `127.0.0.1` (port 8737 by default, `--port` to
  change it), protected by the twelve loopback ingress controls ING-01 to ING-12,
  and opened in the user's browser through a one-time handoff URL.
  Double-clicking the app starts it.
- Copy a summary or a transcript from the dashboard; admonitions in summaries
  render as callouts.
- `fotwd mcp`: a read-only MCP server over stdio for querying the library from
  an agent.
- Exports: one meeting as Markdown, text or JSON (`fotwd export`); a lossless
  library archive and its import (`fotwd export-all`, `fotwd import`); a local
  OKF folder (`fotwd export-okf`); and pushes to a GitHub repository through the
  user's `gh` CLI, syncing each meeting as it finishes and again when its summary
  or saved brief changes. Every meeting shows its own sync state in the
  dashboard, a failed push is retried on a bounded schedule, and a setting syncs
  the whole existing library rather than only new meetings
  ([docs/SHARING_DOCUMENTS.md](docs/SHARING_DOCUMENTS.md#github-sync)).
- `fotw doctor` and `fotwd onboard`, which check capture permissions by
  actually capturing.
- `fotwd.log`, a size-capped diagnostics log beside the library.
- The `fotw-shell` crate: a state machine and AppKit renderer for a menu-bar
  item, a recording pill and global hotkeys. Its AppKit layer has no automated
  coverage; see [crates/fotw-shell/QA.md](crates/fotw-shell/QA.md).

#### Build

- A Cargo workspace pinned to Rust 1.95.0, CI (formatting, clippy, tests on
  Ubuntu and macOS, cargo-deny, and the platform-seam guard), and `justfile`
  recipes to bundle, dev-sign, verify, release-sign and notarize the `.app`.

[Unreleased]: https://github.com/nolanmak/FlyOnTheWall/commits/main

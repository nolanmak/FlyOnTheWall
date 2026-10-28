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
rest yet; only the library database is. Speech-to-text is Deepgram, or
whisper.cpp on the Mac itself. Audio capture is implemented for macOS only: `fotw-audio` has Windows
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
- On-device transcription with whisper.cpp (#33), compiled in with Metal.
  `fotwd stt local` switches to it, after checking the model you downloaded
  against its published SHA-256. Each leg is cut into utterances at its pauses
  and transcribed on a worker thread of its own, so capture never waits on the
  model. `fotwd stt test <wav>` runs a file through the same path. If the
  engine is set to local and the model is missing, the meeting is recorded
  without a transcript and the reason is shown. Nothing falls back to Deepgram.
- A neural speech gate in front of the local model (#242): Silero VAD v5.1.2
  (MIT), installed with every Whisper model, so music, noise and silence no
  longer become "Thank you." lines. Bracketed annotations (`[BLANK_AUDIO]`,
  `(applause)`) and stock phrases the model doubted were speech are dropped.
  Start refuses local transcription while the detector is missing or corrupt.
  **Upgrade step for existing local or backup users:** Start is refused until
  the detector is installed. Run `fotwd stt models install silero-v5.1.2`
  (or, offline, `fotwd stt models import silero-v5.1.2 <file>`), or reinstall
  your Whisper model from Transcription settings, which adds it. The refusal
  message names the same command.
- `fotw-speechd-client` (#243): the protocol and process owner for the
  FluidAudio speech helper that later issues run Parakeet, Silero VAD and the
  streaming engines through
  ([docs/SPEECHD_PROTOCOL.md](docs/SPEECHD_PROTOCOL.md)). It restarts a crashed
  helper at most 3 times in a row, 2 s apart, and fails open streams at once
  instead of waiting on it. Nothing uses it yet.
- `fotw-speechd` (#243), the Swift helper itself
  ([helpers/fotw-speechd](helpers/fotw-speechd/README.md)): Parakeet Unified
  and Silero VAD through FluidAudio, models pinned to exact revisions with
  per-file SHA-256 and installed only by an explicit setup action, and model-free
  fake engines that the Rust protocol tests run against on macOS. Built only
  with `FOTW_FLUIDAUDIO_SOURCE`; `just speechd-real` is its real-model suite.
- Parakeet Unified 0.6B (English, CC BY 4.0) is the default on-device meeting
  engine for new libraries (#244). It runs in the `fotw-speechd` helper behind
  the same Silero speech gate, one batch request per utterance, and every
  segment records `parakeet-unified` and its pinned revision. Install it from
  Transcription settings or with `fotwd stt models install parakeet-unified`;
  Start refuses with that hint while it is missing. A saved Whisper choice is
  left as it was, and Whisper stays available.
  If the helper fails an utterance, it is retried once after the helper
  restarts; if it fails again, that stretch is kept as audio and recorded as
  an untranscribed gap, and the rest of the meeting is still transcribed.
  **Downgrade note:** a speech journal that records such a gap has a `gap`
  field older builds do not read, so an older build cannot reopen that
  session's journal. Re-transcribing saved Parakeet audio is not supported
  yet; the audio is kept.
- Live captions in local mode (#245): word-by-word text in the dashboard
  while people talk, from Parakeet realtime EOU in the speech helper,
  beside the final engine on each leg. Captions are greyed, revised in
  place per utterance, and replaced by the final text; they are never
  saved, exported or summarized. A caption engine that fails three times
  in a row pauses captions for the meeting ("Live captions paused") and
  nothing else. Install the caption model from Transcription settings or with
  `fotwd stt models install parakeet-eou-320ms` (NVIDIA Open Model License);
  captions are on by default once it is installed, can be switched off there,
  and never start the speech helper for a Deepgram meeting.

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
  dashboard, including whether an automatic pass covers it at all — one that
  does not, such as any meeting while the mode is manual, offers **Sync now**
  instead of waiting for a pass that is not coming. A failed push is retried on a
  bounded schedule that survives a restart, and a setting syncs the whole
  existing library rather than only new meetings
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
- `FOTW_SPEECHD=1` (#243) adds the speech helper to the bundle as
  `Contents/Helpers/fotw-speechd`, signed with its own identifier, the hardened
  runtime and no entitlements. `just verify-bundle` fails if it ever carries the
  microphone entitlement, and now also runs `codesign --verify --deep --strict`.
  Without the variable the bundle is unchanged.

[Unreleased]: https://github.com/nolanmak/FlyOnTheWall/commits/main

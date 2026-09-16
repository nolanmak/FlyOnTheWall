# FlyOnTheWall

An open-source, local-first meeting recorder for macOS. It captures system audio and your microphone as two separate streams with **no bot joining the call**, transcribes them live through **Deepgram with your own API key**, and turns each finished meeting into a title, a summary that cites the transcript, and action items whose evidence is checked, using a summary engine you choose. The meeting library is a SQLCipher-encrypted database on your own disk. **The recorded audio beside it is not encrypted yet** — see [Privacy](#privacy).

> **Status: pre-release.** macOS only (14.4 or later), built from source. There are no signed or prebuilt releases. Capture, live Deepgram transcription, the encrypted library, summaries, exports and the web dashboard are implemented and wired together; the largest gaps are listed under [Known gaps](#known-gaps). **[docs/REQUIREMENTS.md](docs/REQUIREMENTS.md)** is the 2026-08 design record, not a description of the current code; work is tracked in [issues](https://github.com/nolanmak/FlyOnTheWall/issues).

### Known gaps

- **Audio is not encrypted at rest** ([#56](https://github.com/nolanmak/FlyOnTheWall/issues/56)). Only the database is.
- **Deepgram is the only speech-to-text provider.** On-device transcription ([#33](https://github.com/nolanmak/FlyOnTheWall/issues/33)) and an ordered failover chain across providers ([#12](https://github.com/nolanmak/FlyOnTheWall/issues/12)) are planned. `fotwd key set` already accepts `elevenlabs` and `openai` keys, but nothing uses them yet.
- **No menu-bar item or floating recording indicator.** Both are written, in `fotw-shell`, but nothing in the app starts them ([#58](https://github.com/nolanmak/FlyOnTheWall/issues/58)). Recording is started, stopped and shown in the dashboard.
- **Notes cannot be typed yet.** The library, search, summaries and exports all handle a meeting's notes, but no shipped screen or command writes them.
- **Windows and Linux** capture backends are empty placeholders (milestone M4).

## The three commitments

1. **No open-core.** No paid tier, no reserved features, ever. (`anarlog` gates hosted models behind Pro; `meetily` gates speaker diarization behind PRO.)
2. **BYO cloud STT as the first-class path** — keys in the OS keychain, audio going only to the endpoint you configured, no vendor relay. Not local Whisper with cloud as an upsell.
3. **Consent as a product feature** — a non-dismissable recording indicator, a disclosure kit, and a jurisdiction warning engine. Not a paragraph in a ToS.

## Features

- **Two-stream capture.** Core Audio process taps record system audio and the default microphone into a crash-safe session on disk as the audio arrives, so a crash, a network stall or a provider outage does not lose the recording. Nothing records until you press **Start** in the dashboard, and Start is enabled only once you tick *everyone consented*.
- **Live transcription.** With a Deepgram key, both streams are transcribed while you record and the transcript appears in the dashboard as it arrives. Without a key the meeting is still recorded, just not transcribed.
- **An encrypted, searchable library.** SQLCipher, with full-text search over titles, transcripts, notes and summaries. Its key lives in the macOS keychain, behind a mandatory [Recovery Key](#your-recovery-key).
- **Summaries.** Each finished meeting gets a title, a summary that cites transcript segments, and action items whose evidence is checked before they are kept. Templates are Markdown files; `fotwd templates install` copies the built-in ones to `~/.flyonthewall/templates` for you to edit.
- **Exports.** `fotwd export <id>` writes one meeting as Markdown, text or JSON. `fotwd export-all <dest> --yes-plaintext` writes a lossless archive of the whole library that `fotwd import` reads back. `fotwd export-okf <dest>` writes a folder of Markdown an agent can index. All of them are plain text.
- **An MCP server.** `fotwd mcp` gives a local agent (Claude Desktop, Cursor and others) three read-only tools over the library, on stdio.
- **GitHub export**, off by default: commits meetings as Markdown to a repository you choose, using your own `gh` login.
- **Audio retention.** The daemon deletes a meeting's audio 30 days after its transcript is ready, and the oldest audio first once the total passes 20 GiB. Transcripts are kept. `fotwd retention` shows what a sweep would delete without deleting anything; `--days` and `--budget-gib` change the two limits.
- **Consent tools.** `fotwd disclose` prints a notice for the meeting chat, a blurb for the calendar invite and a script to say out loud. See [docs/CONSENT.md](docs/CONSENT.md).

### Shareable meeting documents

Finished meetings can prepare an editable document shaped by the conversation's
purpose: a video brief, client recap, project plan, or another suitable format.
Click **Create & download .md** to generate and automatically download a document.
Use **Customize** for purpose and audience, **Review or edit** for changes, or
**Print / PDF** to save a PDF. Automatic
drafts use your configured summary engine and can be turned off in that panel.
Original recordings stay intact. Downloads are explicit; configured GitHub auto-export also syncs saved briefs and summaries. See
[Sharing documents](docs/SHARING_DOCUMENTS.md) for behavior and limits.

### Recording guardrails

UI recordings automatically stop after two hours, with a visible countdown
and a warning in the final five minutes. The deadline is enforced by the
daemon even when the browser is closed, and checked again after laptop sleep.
See [recording guardrails](docs/RECORDING_GUARDRAILS.md) for behavior and the
proposed end-of-call detection policy.

## Privacy

### What is encrypted, and what is not

| Location | Contents | Encrypted at rest |
|---|---|---|
| `db.sqlite3` | meetings, transcripts, notes, summaries, meeting documents, settings | **yes** — SQLCipher, with the key in the macOS keychain |
| `sessions/<id>/` | a meeting not yet archived: raw audio (`system.pcm`, `mic.pcm`) and its transcript (`stt.jsonl`) | **no** |
| `media/<yyyy>/<mm>/<id>/` | archived audio (`system.opus`, `mic.opus`) | **no** ([#56](https://github.com/nolanmak/FlyOnTheWall/issues/56)) |
| `audit.jsonl`, `fotwd.log` | the recording audit trail and the daemon's diagnostics | no |
| anything `export`, `export-all` or `export-okf` writes | the meetings you exported | no, by design; the commands that write files say so |

Nothing marked no needs a key to read: the transcript text in `sessions/<id>/stt.jsonl` and in every export is plain text, `audit.jsonl` and `fotwd.log` are plain-text logs that identify meetings by id (`audit.jsonl` also records export paths, which include meeting titles), and `export-all --audio` copies the audio files unchanged. Paths are relative to the data folder under [Where your data lives](#where-your-data-lives). FileVault encrypts the whole disk while the Mac is off. It does not cover a backup or sync copy of that folder, or another app running as you. Those are the cases the database encryption exists for, and today they expose the audio.

### What leaves your machine

There is no FlyOnTheWall server. Data leaves only for services you configure, under your own accounts:

- **Audio, to Deepgram** — only while recording, and only when a Deepgram key is configured. Both streams go straight to `api.deepgram.com`, and every request carries `mip_opt_out=true`, Deepgram's opt-out from model training.
- **Transcripts and notes, to your summary engine** — for each finished meeting, once an engine is configured. That is the Anthropic API when an Anthropic key is stored, otherwise the `claude` or `codex` CLI you enabled, which sends it to Anthropic or OpenAI through that CLI's own login. Meeting documents use the same engine. With no engine configured nothing is sent, and meetings get a local fallback title.
- **Transcripts, to GitHub** — only if you turn GitHub export on; it is off by default. It uses your `gh` login to commit each meeting's full transcript and notes, its summary and its saved document brief as Markdown to the repository you choose. In auto mode the daemon syncs each meeting once it has finished and been summarised, and syncs it again when its summary or saved brief changes; every meeting shows its own sync state in the dashboard, and a meeting whose push failed is retried on a bounded schedule without being asked. Meetings recorded before auto was switched on are left alone unless you also turn on *sync every meeting in the library*. It refuses any repository GitHub does not confirm is private, unless the export settings store your acknowledgement that it may be public; see [GitHub sync](docs/SHARING_DOCUMENTS.md#github-sync).
- **Library contents, to an agent** — only if you point an agent at `fotwd mcp`. The reading is local; what that agent then sends to its own model is up to the agent.

## Install

There are no prebuilt or signed releases yet. The only way to run FlyOnTheWall is to build it from source on a Mac and sign it with a local development identity, as below.

### Prerequisites

- macOS 14.4 or later.
- Xcode Command Line Tools: `xcode-select --install`. Xcode.app is not required.
- [rustup](https://rustup.rs). The toolchain version is pinned by `rust-toolchain.toml`.
- [just](https://github.com/casey/just), for example `brew install just`.
- cmake, for example `brew install cmake`.
- Node 18 or newer, only for the dashboard's UI tests (`just test`, `just ci`).
- Optional: [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), to run CI's license, ban and source checks locally with `cargo deny check licenses bans sources`.

### Build and check

```sh
git clone https://github.com/nolanmak/FlyOnTheWall.git
cd FlyOnTheWall
rustup toolchain install 1.95.0   # the version pinned in rust-toolchain.toml
just ci                           # fmt, clippy, tests, cargo-deny and the platform-seam checks
```

## First run

```sh
just dev-sign   # once: a persisted self-signed identity, stable across rebuilds
just run        # build, sign and launch FlyOnTheWall.app
```

`just dev-sign` builds `packaging/build/FlyOnTheWall.app` and signs it with a self-signed code-signing identity. The first time, it creates that identity in its own keychain under `~/.fotw-dev-cert`, trusts it for code signing and adds that keychain to your keychain search list. `just dev-unsign` undoes what it changed.

The first `just run` on a machine with no meeting library shows your **Recovery Key** in the terminal and asks you to type two of its groups back, before the app opens (see [Your Recovery Key](#your-recovery-key)). No library is created until you do. `just run` then launches the app through LaunchServices with `open -a … --args serve`, and the dashboard opens in your default browser at `http://127.0.0.1:8737`.

The browser is opened with a one-time link that expires after 30 seconds. To get back to the dashboard later, launch the app again, for example with `open packaging/build/FlyOnTheWall.app`; a running app opens a new authorized tab rather than starting a second daemon. macOS asks for microphone and system-audio permission the first time you press **Start**.

**Record through the app, never from a shell.** macOS attributes the capture grant to the *responsible process*, so a `fotwd` started from a terminal records under Ghostty/iTerm/Terminal's identity, not FlyOnTheWall's — and an unsigned binary can silently inherit the terminal's existing grant and appear to work while producing nothing for your users. See [CONTRIBUTING.md](CONTRIBUTING.md).

### Running `fotwd` commands

Configuration, recovery and export commands do not capture audio, so they are fine to run from a terminal (`fotwd record` is the exception). Use the signed binary inside the bundle, so the keychain sees the same code identity as the app. From the repository root:

```sh
alias fotwd="$PWD/packaging/build/FlyOnTheWall.app/Contents/MacOS/fotwd"
```

### Add a Deepgram key

```sh
fotwd key set deepgram   # paste the key and press return; it is read from stdin, never from argv
fotwd key list           # which providers have a key stored, never the key itself
```

The key goes into the macOS keychain. It is looked up each time you press **Start**, so a running app uses it from the next recording.

### Choose a summary engine

Pick one:

```sh
fotwd key set anthropic                          # the Anthropic API, with your own key
fotwd engine claude-cli --i-acknowledge-egress   # the claude CLI you are logged in to
fotwd engine codex-cli --i-acknowledge-egress    # the codex CLI you are logged in to
```

Run `fotwd engine claude-cli` or `fotwd engine codex-cli` without the flag first: it prints what that CLI sends, to whom, and the provider's training default, and does not enable it. Add `--binary /full/path/to/claude` if the daemon cannot find the CLI. A stored Anthropic key always takes precedence over a CLI engine. `fotwd engine` with no arguments reports what enrichment will actually use, and `fotwd engine off` turns the CLI engine off. The dashboard's **Summaries** panel configures the CLI engines too.

## Your Recovery Key

The meeting library is SQLCipher-encrypted with a 32-byte key that lives in the OS keychain. On first run FlyOnTheWall shows you a **Recovery Key** — `fotw1-` followed by eight groups of four — and will not create the library until you have typed two of those groups back and then the phrase `i have written it down`. It is not a formality:

**If the keychain entry is lost and you do not have that string, nobody can open your library. Not you, not us.** A wiped machine, a restore onto new hardware, or a keychain that no longer recognises the app all produce that state.

The Recovery Key protects the database. It does nothing for the audio files, which are not encrypted (see [Privacy](#privacy)).

Write it on paper. Then:

```sh
fotwd recover --check   # confirm the card in the drawer still works, changes nothing
fotwd recover           # the day the keychain is gone
```

A sealed copy of the library key sits beside the database in `db.sqlite3.recovery`. That file **cannot open anything on its own** — it is the lock, not the key — but it must be backed up together with `db.sqlite3`, because the Recovery Key alone cannot open anything either. The Recovery Key unwraps the sealed file; it is not the database key, so adding or replacing one never re-encrypts your library.

Automation with no terminal refuses to create a library rather than mint one whose key nobody has seen; set `FOTW_RECOVERY_UNATTENDED=print-the-key-to-stdout` if you accept that the key ends up in whatever captures that output.

## Where your data lives

The library and the audio live in `~/Library/Application Support/com.flyonthewall.fotw/`:

| Path | What it is |
|---|---|
| `db.sqlite3`, with its `-wal` and `-shm` files | the encrypted library |
| `db.sqlite3.recovery` | the library key, sealed under your Recovery Key |
| `media/` | archived audio, **unencrypted** |
| `sessions/` | meetings not yet archived, raw audio and transcript, **unencrypted** |
| `audit.jsonl` | who started each recording and the consent warning shown |
| `fotwd.log` | daemon diagnostics |
| `daemon.json` | the running daemon's port and bearer token (mode 0600), rewritten at every start |

Summary templates, once installed, live in `~/.flyonthewall/templates` (or `$FOTW_TEMPLATES_DIR`). Provider keys and the library key are in the macOS keychain, not in either folder.

### What to back up

- **The whole `com.flyonthewall.fotw` folder.** At minimum `db.sqlite3` and `db.sqlite3.recovery` together: on a new machine the Recovery Key needs the sealed file to open the database. A backup tool that works from a disk snapshot, as Time Machine does, captures `db.sqlite3` and its `-wal` file at the same moment; a plain copy made while the daemon is writing may not.
- **`~/.flyonthewall/templates`**, if you have edited templates.
- **Your Recovery Key**, on paper, somewhere other than this Mac and its backups.

A backup of this folder contains your meeting audio in the clear, and retention deletes audio from `media/` on its own schedule, so a backup may hold audio the app has already removed.

## Development

`just --list` shows the recipes. The whole pipeline is testable with **no audio device and no GUI** — `FileAudioSource` replays a WAV fixture through the real seam at a speed multiplier, so a 90-minute meeting runs inside a CI step. [CONTRIBUTING.md](CONTRIBUTING.md) covers signing, permissions and what CI cannot test.

## Stack

Pure Rust. A daemon (`fotwd`) owns capture, STT and storage, serves the dashboard on `127.0.0.1` to your own browser, and is also the command line for everything the dashboard does not cover. A separate `fotw` binary has two capture diagnostics, `doctor` and `record`. A thin AppKit shell in Rust (`fotw-shell`) for the menu-bar item and the recording indicator is written but not yet started by the app ([#58](https://github.com/nolanmak/FlyOnTheWall/issues/58)). It all runs from one `.app` bundle, because the capture grant belongs to a signed bundle — a macOS TCC requirement, not a UI framework choice. The release path (`just release-sign`, `just notarize`) exists, but no signed build has been published. No Tauri, no Electron, no Swift, no Xcode.

## Scope

macOS 14.4+ first (Core Audio process taps, Developer ID, no Mac App Store). The platform seam compiles for other targets in CI, but the Windows and Linux capture backends are empty placeholders until M4.

## License

FlyOnTheWall is licensed under the Apache License 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). No GPL/AGPL code enters the tree, and `cargo deny check licenses` enforces the dependency license allowlist in CI.

The app icon (`packaging/AppIcon.svg`, `packaging/AppIcon.icns`) and the dashboard favicon (`crates/fotw-web/ui/favicon.svg`) are derived from ["Fly"](https://game-icons.net/1x1/delapouite/fly.html) by Delapouite, licensed under [CC BY 3.0](https://creativecommons.org/licenses/by/3.0/). The fly is recoloured green and set on a transparent background for the favicon, and on a rounded dark tile for the app icon.

Notices and license texts for the third-party Rust crates and the C libraries compiled into the app (SQLCipher, SQLite, OpenSSL and libopus) are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), which `just licenses` regenerates.

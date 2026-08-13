# FlyOnTheWall

An open-source, local-first meeting recorder. Captures system audio + microphone with **no bot joining the call**, transcribes with **your own API key** (Deepgram, ElevenLabs, OpenAI, or fully on-device), and turns the sparse notes you typed during the call into a grounded, citation-backed document — all stored in an encrypted SQLite database on your own disk.

> **Status: early build.** The platform seam and the packaging pipeline are in and green; capture, STT and storage are not written yet. Full requirements and technical design in **[docs/REQUIREMENTS.md](docs/REQUIREMENTS.md)**; work is tracked in [issues](https://github.com/nolanmak/FlyOnTheWall/issues).

## The three commitments

1. **No open-core.** No paid tier, no reserved features, ever. (`anarlog` gates hosted models behind Pro; `meetily` gates speaker diarization behind PRO.)
2. **BYO cloud STT as the first-class path** — keys in the OS keychain, audio going only to the endpoint you configured, no vendor relay. Not local Whisper with cloud as an upsell.
3. **Consent as a product feature** — a non-dismissable recording indicator, a disclosure kit, and a jurisdiction warning engine. Not a paragraph in a ToS.

## What it is not

Not cheaper than Granola. At default settings BYO-key runs ~$0.88 per meeting-hour, so **anything past ~16 meeting-hours a month costs more than a $14/mo subscription**. The pitch is ownership, model choice, editable transcripts, retained audio, no 30-day amnesia, and no training-by-default — not price. Full breakdown in [§12 Cost model](docs/REQUIREMENTS.md#12-cost-model).

## Stack

Pure Rust. A daemon (`fotwd`) owns capture, STT, and storage and serves a web UI on `127.0.0.1` to your own browser; a thin AppKit shell in Rust owns the menu-bar item and the recording indicator; a `fotw` CLI is a first-class client. All of it ships inside one signed, notarized `.app` — because that is a macOS TCC requirement, not a UI framework choice. No Tauri, no Electron, no Swift, no Xcode.

## Scope

macOS 14.4+ first (Core Audio process taps, Developer ID, no Mac App Store). The platform seam for Windows and Linux is built on day one and compiles in CI, but those implementations are M4.

## Building

```sh
xcode-select --install          # Command Line Tools — Xcode.app is NOT required
rustup toolchain install 1.95.0 # pinned in rust-toolchain.toml
just ci                         # fmt + clippy + tests + the platform-seam guard
```

`just --list` shows the rest. The whole pipeline is testable with **no audio device and no GUI** — `FileAudioSource` replays a WAV fixture through the real seam at a speed multiplier, so a 90-minute meeting runs inside a CI step.

### Running it on macOS

```sh
just dev-sign   # persisted self-signed identity, stable across rebuilds
just run        # launches the .app via LaunchServices
```

**Never run `./target/debug/fotwd` directly.** macOS attributes the TCC grant to the *responsible process*, so a binary launched from a terminal records under Ghostty/iTerm/Terminal's identity, not ours — and an unsigned binary can silently inherit the terminal's existing grant and appear to work while producing nothing for your users. See [CONTRIBUTING.md](CONTRIBUTING.md).

### Your Recovery Key

The meeting library is SQLCipher-encrypted with a 32-byte key that lives in the OS keychain. On first run FlyOnTheWall shows you a **Recovery Key** — `fotw1-` followed by eight groups of four — and will not create the library until you have typed two of those groups back. It is not a formality:

**If the keychain entry is lost and you do not have that string, nobody can open your library. Not you, not us.** A wiped machine, a restore onto new hardware, or a keychain that no longer recognises the app all produce that state.

Write it on paper. Then:

```sh
fotwd recover --check   # confirm the card in the drawer still works, changes nothing
fotwd recover           # the day the keychain is gone
```

A sealed copy of the library key sits beside the database in `db.sqlite3.recovery`. That file **cannot open anything on its own** — it is the lock, not the key — but it must be backed up together with `db.sqlite3`, because the Recovery Key alone cannot open anything either. The key unwraps the sealed file; it does not replace the database key, so rotating it never re-encrypts your library.

Automation with no terminal refuses to create a library rather than mint one whose key nobody has seen; set `FOTW_RECOVERY_UNATTENDED=print-the-key-to-stdout` if you accept that the key ends up in whatever captures that output.

## License

Apache-2.0 (planned) — permissive, compatible with every identified dependency, with a patent grant and a NOTICE mechanism for vendored attribution. No GPL/AGPL code enters the tree.

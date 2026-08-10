# FlyOnTheWall

An open-source, local-first meeting recorder. Captures system audio + microphone with **no bot joining the call**, transcribes with **your own API key** (Deepgram, ElevenLabs, OpenAI, or fully on-device), and turns the sparse notes you typed during the call into a grounded, citation-backed document — all stored in an encrypted SQLite database on your own disk.

> **Status: scoping.** No code yet. The full requirements and technical design live in **[docs/REQUIREMENTS.md](docs/REQUIREMENTS.md)**.

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

## License

Apache-2.0 (planned) — permissive, compatible with every identified dependency, with a patent grant and a NOTICE mechanism for vendored attribution. No GPL/AGPL code enters the tree.

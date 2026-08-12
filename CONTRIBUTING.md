# Contributing

## Provenance of code you submit

By opening a pull request you confirm that you wrote the code, or that you have
stated its origin and license in the PR description.

**Do not copy code from these projects.** They are the most visible references in
this space and every one of them is unusable here:

| Project | Why |
|---|---|
| `screenpipe` | relicensed to a proprietary commercial license |
| `natively` | source-available, non-commercial only |
| `cheating-daddy`, `pluely` | GPL-3.0 |
| `amurex` | AGPL-3.0, abandoned since 2025-05 |
| `audiotee` | **no LICENSE file at all** — all rights reserved |

`cargo deny check licenses` runs in CI and will fail the build on a
non-allowlisted dependency. It cannot catch a copy-pasted snippet — that part is
on you.

## Building

```sh
rustup toolchain install 1.95.0   # pinned in rust-toolchain.toml
cargo test --workspace
```

The whole pipeline is testable with no Mac-specific hardware and no audio device
via `FileAudioSource` and the mock STT server. If your change is in
`fotw-audio/src/platform/macos`, it is not covered by CI — say so in the PR and
note what you tested manually.

## ⚠️ macOS: your dev machine will lie to you about permissions

This is the single most confusing thing about working on this project.

Capturing system audio requires a TCC grant, and **TCC keys that grant to the
code's Designated Requirement**. An ad-hoc signature (`codesign -s -`) produces a
cdhash-based DR that changes on *every rebuild*, so macOS treats each build as a
brand-new app.

Worse: an unsigned binary run from a terminal can **inherit the terminal's own
grant** and capture real audio with no prompt at all. You will conclude that
capture works. It does not work for your users.

So:

- Build and run through the `.app` bundle, never `./target/debug/fotwd` directly,
  and never document the bare binary as a way to run it.
- Use `just dev-sign`, which creates or reuses a *persisted* self-signed identity
  outside the build tree and prints the resulting DR.
- To reset permission state: `tccutil reset AudioCapture com.flyonthewall.fotw`
  (the service is `AudioCapture` — `SystemAudioCaptureRequests`, which appears in
  several 2026 blog posts, does not exist).
- `fotw doctor` runs a real one-second tap and reports whether non-zero samples
  arrived. There is no public API to query this permission, so a round-trip test
  is the only truthful answer available.
- `fotwd onboard` does the same for both legs *and reads the environment it ran
  in*: it plays a test tone, counts what came back, and then tells you whether
  the result means anything. Run from a shell it will say **"THIS RESULT IS NOT
  EVIDENCE"** even when audio arrived, because the grant it used may be your
  terminal's. That is the intended output of a development build, not a bug.
- `fotwd detect [seconds]` prints what meeting detection can see and what it
  decides. It cannot start a recording — it holds the state machine and asserts
  that nothing it does produces a `StartCapture`.

## Tests

New behavior comes with a test. Bug fixes come with a test that fails before the
fix. Where a requirement in `docs/REQUIREMENTS.md` has an explicit acceptance
criterion, the test should assert that criterion.

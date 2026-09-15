# Contributing

FlyOnTheWall is a macOS meeting recorder written in Rust. This file covers what
to install, how to build and test, what the dev-signing setup changes on your
Mac, how to run the app from source, and what a pull request needs.

**Found a security problem?** Do not open an issue or a pull request. Report it
privately, as [SECURITY.md](SECURITY.md) describes, through
<https://github.com/nolanmak/FlyOnTheWall/security/advisories/new>.

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

## Prerequisites

| Tool | Why | Install |
|---|---|---|
| rustup | `rust-toolchain.toml` pins the toolchain (1.95.0, with rustfmt and clippy), and rustup applies it inside this repository | <https://rustup.rs>, then `rustup toolchain install 1.95.0` |
| Xcode Command Line Tools | The C compiler and macOS SDK that the native dependencies (SQLCipher, OpenSSL, Opus) build with. Xcode.app is not needed | `xcode-select --install` |
| just | Runs the recipes in `justfile` | `brew install just` or `cargo install just` |
| cmake | Builds the Opus library that `opusic-sys` bundles. The Command Line Tools do not include it | `brew install cmake` |
| Node 18 or newer | The web UI tests only (`node --test`). There is nothing to `npm install` | `brew install node`, or any Node installer |
| cargo-deny (optional) | The dependency license check CI runs. `just ci` skips it, and says so, when it is missing | `cargo install --locked cargo-deny` |
| Windows target (optional) | The Windows cross-check CI runs. `just ci` skips it, and says so, when it is missing | `rustup target add x86_64-pc-windows-msvc` |

`just dev-sign` also calls `openssl`. The one macOS ships at `/usr/bin/openssl`
(LibreSSL) works, and so does OpenSSL 3; Homebrew's OpenSSL is not required.

The app itself needs macOS 14.4 or later. The test suite also runs on Linux (CI
runs it on Ubuntu); there you need a C compiler, `make` and `perl` for the
vendored OpenSSL that SQLCipher is built against, plus cmake, just and Node.

## Building and testing

```sh
just ci
```

That runs what CI runs (`.github/workflows/ci.yml`), in CI's order:

| Recipe | What it runs |
|---|---|
| `just lint` | `cargo fmt --all --check`, then `cargo clippy --workspace --all-targets -- -D warnings` |
| `just test` | `cargo test --workspace`, then `node --test crates/fotw-web/tests/ui/*.cjs` |
| `just deny` | `cargo deny check licenses bans sources`, or a "skipped" line if cargo-deny is not installed |
| `just seam` | `cargo check -p fotw-audio --target x86_64-pc-windows-msvc`, or a "skipped" line if that target is not installed; then a grep that fails if a macOS audio type appears outside `crates/fotw-audio/src/platform/macos/` |

Where CI still differs from a local `just ci`:

- CI runs the tests on both `ubuntu-latest` and `macos-15`.
- CI sets `RUSTFLAGS=-D warnings` on every job, and its lint job runs on
  Ubuntu. Code compiled only off macOS (`cfg(not(target_os = "macos"))`) is
  therefore linted in CI but not by `just lint` on a Mac.
- CI also runs `cargo deny check advisories` as a separate job on every push,
  every pull request and once a week, so a newly published RustSec advisory
  turns only that check red. Run it locally with `cargo deny check advisories`.
- A check that is skipped locally still runs in CI. Install cargo-deny and the
  Windows target (see [Prerequisites](#prerequisites)) to see those failures
  before you push.

The recipes build with 3 parallel cargo jobs so the machine stays usable; set
`CARGO_BUILD_JOBS` to change that.

### Dependency changes and third-party notices

`THIRD_PARTY_NOTICES.md` carries the license texts for everything compiled into
the app, and `just bundle` copies it into the `.app`. If your change touches
`Cargo.lock`, including a Dependabot update, run `just licenses` and commit the
regenerated file. The recipe installs cargo-about 0.9.2 if it is missing and
fails if a different cargo-about version is installed. It also fails when a
libsqlite3-sys, openssl-src or opusic-sys bump, or fotwd no longer depending on
one of them, means the C library section in `packaging/licenses/about.hbs` needs
re-checking.

The whole pipeline is testable with no Mac-specific hardware and no audio device
via `FileAudioSource` and the mock STT server. If your change is in
`fotw-audio/src/platform/macos`, it is not covered by CI — say so in the PR and
note what you tested manually.

## Running the app on macOS

### ⚠️ Your dev machine will lie to you about permissions

This is the single most confusing thing about working on this project.

Capturing system audio requires a TCC grant, and **TCC keys that grant to the
code's Designated Requirement**. An ad-hoc signature (`codesign -s -`) produces a
cdhash-based DR that changes on *every rebuild*, so macOS treats each build as a
brand-new app.

Worse: an unsigned binary run from a terminal can **inherit the terminal's own
grant** and capture real audio with no prompt at all. You will conclude that
capture works. It does not work for your users.

So:

- Build and run through the `.app` bundle with `just run`, never
  `./target/debug/fotwd` directly, and never document the bare binary as a way
  to run it. The one exception is the first-run step [below](#first-run), which
  records nothing.
- `just dev-sign`, which `just run` calls, creates or reuses a *persisted*
  self-signed identity outside the build tree and prints the resulting DR.
  [What it changes on your machine](#what-just-dev-sign-changes-on-your-machine)
  is listed below.
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

### What `just dev-sign` changes on your machine

> **Read this before your first `just dev-sign`.** It makes four changes outside
> this repository, and they stay until you run
> [`just dev-unsign`](#just-dev-unsign).

1. **A folder**: `~/.fotw-dev-cert`, or the path in `FOTW_DEV_CERT_DIR`. Give
   that variable a folder of its own, because dev-sign makes it owner-only
   (mode 700), and dev-unsign removes the files dev-sign put there and then the
   folder itself if nothing else is in it. It holds:
   - `fotw-dev.keychain-db`, a keychain containing a self-signed code-signing
     certificate named "FlyOnTheWall Dev" (RSA 2048, valid for 10 years) and
     its private key. It locks after six hours and when the Mac sleeps;
     dev-sign unlocks it on every run.
   - `cert.pem`, the public certificate.
   - possibly a zero-length `.fl` lock file, which the Security framework
     leaves beside a keychain.

   Folders made by earlier versions of dev-sign also hold `key.pem` (the same
   private key, unencrypted) and `dev.p12`. dev-sign no longer writes those, and
   dev-unsign removes them.
2. **A keychain search list entry.** The keychain is added to your user search
   list (`security list-keychains -d user`), keeping the existing entries,
   because `codesign` finds identities only through that list.
3. **A trust setting.** The certificate is trusted for code signing, and for
   nothing else, in your user trust settings (`security add-trusted-cert -r
   trustRoot -p codeSign`); `codesign` ignores an untrusted identity. No
   administrator trust settings are changed and nothing runs under `sudo`.
4. **Signatures.** `packaging/build/FlyOnTheWall.app`, `target/debug/fotwd`,
   `target/debug/fotw` and the built examples are signed with that identity,
   all under the identifier `com.flyonthewall.fotw`.

The keychain password is `fotw`, and it is in the justfile on purpose: the
recipe has to unlock the keychain without asking you, so any password would be
published there. The owner-only folder protects the private key, not the
password. Anyone who can read that folder can sign binaries that match your dev
build's designated requirement, and that requirement is what the dev build's
TCC grants and its access to the library key in your login keychain are tied
to. Do not copy the folder anywhere other people or other accounts can read it,
and keep it out of shared backups. The identity is made on your machine and is
never used to sign a release.

### `just dev-unsign`

Reverses those changes, in this order:

1. Removes the code-signing trust setting. macOS asks you to authenticate in a
   dialog for this, so run it in a terminal on the Mac itself. Over SSH nobody
   can answer the dialog, so the recipe stops there before changing anything.
2. Removes the dev keychain from your search list, keeping every other entry in
   order.
3. Deletes the dev keychain, and the private key with it.
4. Deletes `cert.pem`, `key.pem`, `dev.p12`, the lock file and any scratch
   folder an interrupted dev-sign left, then the folder itself if nothing else
   is in it.

It leaves alone:

- the bundle and binaries already signed with the identity (`just clean`
  removes them);
- the TCC grants for `com.flyonthewall.fotw` (`tccutil reset AudioCapture
  com.flyonthewall.fotw` and `tccutil reset Microphone com.flyonthewall.fotw`);
- your meeting library and its `db:masterkey` item in the login keychain. Do
  not delete that item unless you have the Recovery Key, or the library can
  never be opened again.

Running `just dev-sign` again afterwards creates a new identity, and so a new
designated requirement: macOS asks for audio permission again, and asks you to
approve the app's access to its library key.

### First run

```sh
just run
```

1. Runs `just dev-sign`, which builds a debug bundle and signs it.
2. If there is no meeting library yet, meaning no
   `~/Library/Application Support/com.flyonthewall.fotw/db.sqlite3`, it runs
   the bundle's own `fotwd list` in your terminal. The library database is
   encrypted with a key kept in your login keychain (recorded audio is not; see
   [README.md](README.md#privacy)), and FlyOnTheWall will not create it until
   you have seen its Recovery Key: the key is printed, you type two of its
   groups back, and then you type the phrase `i have written it down`. Write
   the key on paper. If you stop partway, nothing is created and `just run`
   stops there too.
3. Launches the app with `open -a "$(pwd)/packaging/build/FlyOnTheWall.app"
   --args serve`. The daemon serves the UI on `127.0.0.1` and opens it in your
   browser.

Step 2 happens in the terminal because a LaunchServices launch has no terminal
to show the Recovery Key in: launched that way on a first run, the daemon
refuses to create a library and records why only in its log. It is the one
place a recipe runs the bundled binary from a shell, and it does not break the
rule above, because that rule is about audio permissions being attributed to
whoever launched the process. `list` records nothing and asks for no permission.
It is also the same signed binary the app runs, so the app reads the library
key it stores without an approval dialog. Every launch after that goes through
`open -a`.

If `just run` finishes and no browser tab opens, read
`~/Library/Application Support/com.flyonthewall.fotw/fotwd.log`. For example, a
library whose key is no longer in the keychain is refused there, with
instructions for `fotwd recover` (run it as
`packaging/build/FlyOnTheWall.app/Contents/MacOS/fotwd recover`; it records
nothing either). [README.md](README.md) explains the Recovery Key and what to
back up.

## Tests

New behavior comes with a test. Bug fixes come with a test that fails before the
fix. Where a requirement in `docs/REQUIREMENTS.md` has an explicit acceptance
criterion, the test should assert that criterion.

### Tests CI does not run

Some tests need things CI does not have: the real OS keychain, an AI CLI signed
in to your subscription, or a GitHub repository. Unless you switch them on they
print "skipped" and pass, so a green `cargo test --workspace` says nothing about
them. If your change touches the code one of them covers, run it and say so in
the pull request.

#### `FOTW_KEYCHAIN_TESTS=1`: the real OS keychain

```sh
FOTW_KEYCHAIN_TESTS=1 cargo test -p fotw-secrets
```

Runs `os_store_round_trips_against_the_real_keychain` (in
`crates/fotw-secrets/src/store.rs`) and the KEY-01 test
`no_key_material_reaches_disk_with_the_os_keychain` (in
`crates/fotw-secrets/tests/no_plaintext_on_disk.rs`) against the OS keychain.
Run it when you change the key store in `crates/fotw-secrets`.

> **Warning: this can lock you out of a real library.** Both tests write under
> FlyOnTheWall's real keychain service, `com.flyonthewall.fotw`. The KEY-01 test
> writes a test value to every account the app uses, `db:masterkey` and the
> four `apikey:*` entries, and deletes them all at the end. On a user account
> where FlyOnTheWall already has a library, that replaces the library's master
> key and then deletes it. Run these tests only in a user account that has no
> FlyOnTheWall library and no stored API keys.

On macOS, cargo's test binaries are not signed with the dev identity, and the
code notes that such a binary writing to the login keychain can raise an
interactive prompt. Run the tests from a desktop session where you can answer
it, not over SSH.

#### `FOTW_ENGINE_LIVE=1`: let a test reach your real `claude` or `codex`

This is not a test of its own. In the test build (the `test-guards` feature),
`crates/fotwd/src/engine.rs` panics when a test configures an engine whose file
name is a real CLI's, such as `claude`, at a path with no executable file. The
engine lookup falls back to searching this machine by file name, so without the
guard it would find your installed CLI and run it with the fixture's transcript
on its stdin (issue #83). This variable lifts the guard for a test you mean to
run against a real engine, which spends your subscription. The tests that exist
to check the guard stop with a "skipped" panic instead. Set it for the one test
you mean:

```sh
FOTW_ENGINE_LIVE=1 cargo test -p fotwd --test <file> -- <test name>
```

#### `FOTW_CODEX_LIVE=1`: the real Codex CLI

```sh
FOTW_CODEX_LIVE=1 cargo test -p fotwd --test codex_live -- --nocapture
```

Sends a tiny fixture transcript through the production Codex adapter and CLI
runner, and asserts that an answer comes back. It needs `codex` installed and
signed in, and it uses your Codex subscription. The binary defaults to
`/Applications/Codex.app/Contents/Resources/codex`; set `FOTW_CODEX_BIN` to use
another. Run it when you change `crates/fotw-summarize/src/codex_cli.rs` or the
CLI runner in `crates/fotwd/src/engine.rs`.

#### `FOTW_GH_LIVE=owner/repo`: a real GitHub push

```sh
FOTW_GH_LIVE=owner/scratch-repo cargo test -p fotwd --test github_live -- --nocapture
```

Commits a fixture transcript to that repository with the real `gh`, then
commits it again, which checks the create and the update paths against GitHub
itself. It also commits `fotw-qa/index.md` and `fotw-qa/log.md`. Use a private
scratch repository you own, with `gh` signed in to an account that can push to
it: export refuses a repository GitHub does not report as private. Run it when you change `crates/fotwd/src/github.rs`.

## Real meeting data never goes in the repository

Fixtures, tests, docs, screenshots, issues and pull requests use invented
meetings, invented people and fake keys, the way
`fotw_summarize::testing::sample_meeting()` does. Never commit or paste:

- recordings, transcripts, summaries or notes from a real meeting, or the names
  of the people in one;
- a real API key or Recovery Key;
- a `db.sqlite3`, its `.recovery` file, session audio, or a `fotwd.log` from a
  real library.

If a bug only reproduces with real data, describe its shape (length, languages,
number of speakers) or build a fake that reproduces it.

## Pull requests

1. For anything bigger than a small fix, open an issue first so the approach is
   agreed before you write the code.
2. Fork the repository and branch from `main`. Keep each pull request to one
   change.
3. Run `just ci` before you push. CI has to be green on the pull request.
4. In the description, say what you tested by hand (platform code under
   `fotw-audio/src/platform/macos`, anything you checked through `just run`)
   and which of the [tests CI does not run](#tests-ci-does-not-run) you ran.
5. The maintainer reviews and merges. Push changes from review to the same
   branch.

### Commit messages

Write a short subject in the imperative mood, in sentence case, with no trailing
period and no prefix, such as "Show a spinner while a meeting document is
generated". Add a body when the diff does not explain itself, and use it to say
why the change is needed, not only what it does.

## Design record

`docs/REQUIREMENTS.md` is the design record: the requirements, the decisions
behind them and the alternatives that were rejected. Where it and the code
differ, the code is authoritative. If you find a difference, correct the
document in your pull request or point it out in an issue.

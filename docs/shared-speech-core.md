# Shared speech core migration (#119)

## Inventory and ownership

Baseline mains: FlyOnTheWall ca83343; Murmur 3edd3c4. Murmur's vendor snapshot was
5cfe8562877823fa5403a3e9cd340996d575ce33. The offline-suite changes are reviewed
against these baselines, not treated as a completed extraction.

| Contract | Maintained owner | Consumer policy |
| --- | --- | --- |
| Credential addresses, secret wrappers, redaction, OS key store | fotw-secrets | Murmur resolves explicit process/.env settings before keychain; FlyOnTheWall does not acquire that policy |
| Audio format/callback/frame timestamps and platform taps | fotw-audio | Each app owns capture consent, device recovery, deadlines and generation fencing |
| PCM resampling | fotw-dsp | Pipeline re-exports the shared implementation; Murmur consumes the same crate |
| Transcript segments, typed errors, provider wire protocol | fotw-stt | Each app chooses retries, persistence, fallback and delivery |
| Dictation final drain | Murmur streaming.rs | CloseStream followed by final Metadata; do not replace with meeting socket-close policy |
| Meeting WAL/recovery | FlyOnTheWall daemon | Never move meeting data or consent policy into Murmur |
| Local HTTP Whisper and insertion | Murmur | Local-only skips credential resolution; original-field insertion remains app-owned |
| Local in-process Whisper and bounded queues | fotw-stt local adapter | Whisper runtime is opt-in; capability extraction still pending |

## First migrated contract: credentials

The secrets source was byte-for-byte identical between Murmur's vendor copy and
FlyOnTheWall before migration. Murmur consumes an immutable Git revision and its
Cargo.lock rather than a sibling path. No package or new repository is published.
Original Apache-2.0 LICENSE/NOTICE remain in both repositories.

`fotw-secrets` exposes a `conformance` feature for consumer tests only. Both apps
run `conformance::credentials()` against their actual resolved implementation.
It uses synthetic strings and InMemoryKeyStore, never an OS keychain. An injected
Authorization-header redaction regression made this suite fail; restoring the
implementation makes it pass. Existing library tests still own the broader
recovery/encryption contract. Consumer config/local/fallback tests establish that
Murmur's provider policy is not changed by sharing the dependency.

Upgrade by choosing a reviewed full commit SHA, updating both dependency and
dev-dependency pins in Murmur, updating Cargo.lock, and running both consumers'
required checks. Roll back those manifest/lockfile changes together. Never pin a
moving branch or silently update to upstream main. The `murmur-compatibility.yml` workflow patches fotw-audio, fotw-stt and fotw-secrets into
Murmur revision `882b106e3e6ebadc3f50f276b316a8c2a732593f`, then runs its workspace
tests on macOS and Linux. The disposable compatibility lockfile may update for
the source substitution; ordinary consumer CI continues to use `--locked`.

Murmur now consumes all three crates at an immutable Git revision. The shared
audio/STT conformance suite covers format validation, provider timestamps,
interim/final normalization, stream-end finalization, bounded local PCM, silence
and cancellation. Injected timestamp, finalization and late-final regressions
fail these tests. Murmur retains its separately tested CloseStream/Metadata
transport. Its full workspace tests, formatting and Clippy pass after removing
the vendor copies. The production dependency graph excludes whisper-rs.
Provider capabilities and complete hosted compatibility gates remain outstanding.
Shared Rust source does not share loaded model memory. Sharing model workers is
separate #120 work, conditional on measured contention.

Validation of the first migration: Murmur builds from a fresh standalone clone
with `cargo check --locked`; its full workspace tests and Clippy pass with the
Git pin. A disposable clean clone also passed the full workspace with the same
producer path substitution used by the compatibility workflow. No OS keychain
test was enabled. Hosted compatibility checks remain required on each revision.

## Shared PCM conversion

`fotw-dsp` owns the formerly identical resampler copies without pulling meeting
WAL, Opus, capture or model runtimes into dictation. `fotw-pipeline::resample`
re-exports the API to preserve existing callers. Existing anti-aliasing,
passband, Bluetooth-rate, downmix and clamping tests run against the new owner
and the pipeline public API. The opt-in `conformance::streaming()` contract
checks callback-size independence (including split stereo frames), output
length, finite samples and signed little-endian PCM across 8–96 kHz inputs.
Dropping pending samples between callbacks makes that contract fail.

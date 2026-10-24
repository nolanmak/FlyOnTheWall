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
moving branch or silently update to upstream main. The `murmur-compatibility.yml` workflow patches fotw-audio, fotw-stt, fotw-secrets and fotw-dsp into
Murmur revision `85df901f00a5d68e55466f0717cf68694373b48b`, then runs its workspace
tests on macOS and Linux. The disposable compatibility lockfile may update for
the source substitution; ordinary consumer CI continues to use `--locked`.

Murmur now consumes all four crates at an immutable Git revision. The shared
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

## Local language validation

The in-process Whisper adapter validates language codes against the installed
runtime before opening a model. It then checks `WhisperContext::is_multilingual`
from model metadata; file names are not evidence of language support. An
English-only model accepts English and resolves automatic selection to English;
other explicit languages produce a non-retryable Unsupported error. The local
setup command loads and validates the selected model/language before saving the
configuration. Session workers repeat validation when creating a recognizer.
This does not yet implement a full pre-capture readiness gate for every meeting
entry point, or the shared language/capability UI.

Validation: the new normalization/capability test failed before implementation;
local adapter unit tests and settings regressions pass after the change. The
ignored `real_english_model_rejects_non_english_from_metadata` test was also run
with the installed base.en artifact (SHA-256
`a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002`), under the name
`model.bin`, and passed. It reads model metadata without capturing or uploading
audio. Normal CI requires no weights; rerun the explicit model test with
`FOTW_WHISPER_ENGLISH_MODEL` pointing to an English-only model and
`cargo test -p fotw-stt --features local-whisper --lib real_english_model -- --ignored`.

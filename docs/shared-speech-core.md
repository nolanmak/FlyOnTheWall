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


## Explicit credential stores and Linux backend compatibility

`fotw-secrets` now initializes a private, cached `keyring-core` store rather
than installing an all-in-one keyring global default. macOS still selects
`apple-native-keyring-store::keychain::Store`; Windows still selects
`windows-native-keyring-store::Store`. Linux selects the DBus Secret Service
backend. Service/account strings, missing-entry handling, error redaction and
operation deadlines remain at their existing boundaries. Initialization probes
store availability without a test credential write; unavailable stores refuse
operations without a plaintext fallback. As before, initialization failure is
cached for the process lifetime.

This removes the retired `zbus_names`, `zvariant` and `zvariant_derive` crates
from the application dependency graph. The observed local
`cargo deny check advisories` failed before this change with
RUSTSEC-2026-0370/0371/0372 and passes after it; no advisory was suppressed.
Compatible upgrades of the prior backend still required zbus 5. Linux uses
system libdbus, requiring `libdbus-1-dev` and `pkg-config` at build time. We do
not compile or distribute the old C sources bundled in libdbus-sys.

Before changing production, the isolated interoperability probe passed in
Linux CI job 120843894681 at 5047db3. It tested all byte values, existing-key
reads, replacement visible through the old backend, one item after updating,
account isolation, new-key creation and cross-backend deletion. It used a
private bus and temporary Secret Service with synthetic service/account names.
The follow-up probe compiles the actual production platform adapter and checks
missing-service refusal in a fresh process. Its separate workspace intentionally
retains the old backend for compatibility tests; it is not linked into fotwd.
See `tests/keyring-interop/README.md` and `scripts/test-keyring-interop.sh`.

Native user-account keychain tests were not enabled. Existing Mac grants and
real stored keys were not accessed for this migration. The Linux production
adapter, hosted compatibility matrix and native release gates still require
verification at the resulting revision before approval.


## Native microphone format validation

Microphone startup now reads the input stream's actual virtual ASBD, rather
than combining nominal sample rate with an assumed mono channel count or
falling back to 48 kHz when probing fails. It accepts a plausible integral
sample rate and native packed Float32 with consistent frame/packet sizes.
Interleaved multichannel and mono Float32 are supported; planar multichannel
and other encodings return FormatUnsupported rather than being misinterpreted.
Supporting those layouts requires a separate conversion path.

The IO callback verifies buffer count, channels and complete frame alignment
before reading samples. A mismatch emits the allocation-free
`TapError::StreamFormatChanged` and delivers no PCM. This is a transient device
fault suitable for rebuild. It does not advance the frame counter or report an
empty buffer as healthy delivery. The error introduces no platform types into
the shared seam. It does not by itself add native rate-change listeners or wire
all consumer recovery paths; those remain #121/#61 work.

Synthetic native tests first reproduced stereo reported as mono, acceptance of
invalid/unknown descriptors and changed-layout audio appended under old metadata.
The passing tests preserve stereo frame timing, reject incompatible descriptors,
and verify that rejected callbacks neither append samples nor refresh delivery
health. No hardware microphone was opened for these tests. Physical USB/Bluetooth
format changes and rebuilding after them remain native acceptance gates.

Before updating Murmur's producer pin, its capture conversion must also retain
partial stereo frames across ring-drain boundaries. FlyOnTheWall already passes
interleaved frames through the streaming resampler before downmixing; Murmur's
current downmix-before-resample path can discard a split frame. This follow-up
is required for the new multichannel format reporting, not completed by these
backend tests alone.

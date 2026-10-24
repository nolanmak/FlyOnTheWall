# Shared speech core migration (#119)

## Inventory and ownership

Baseline mains: FlyOnTheWall ca83343; Murmur 3edd3c4. Murmur's vendor snapshot was
5cfe8562877823fa5403a3e9cd340996d575ce33. The offline-suite changes are reviewed
against these baselines, not treated as a completed extraction.

| Contract | Maintained owner | Consumer policy |
| --- | --- | --- |
| Credential addresses, secret wrappers, redaction, OS key store | fotw-secrets | Murmur resolves explicit process/.env settings before keychain; FlyOnTheWall does not acquire that policy |
| Audio format/callback/frame timestamps and platform taps | fotw-audio | Each app owns capture consent, device recovery, deadlines and generation fencing |
| PCM resampling | fotw-pipeline today; Murmur source copy | Extraction still pending; keep rates, phase and channel behavior tested |
| Transcript segments, typed errors, provider wire protocol | fotw-stt | Each app chooses retries, persistence, fallback and delivery |
| Dictation final drain | Murmur streaming.rs | CloseStream followed by final Metadata; do not replace with meeting socket-close policy |
| Meeting WAL/recovery | FlyOnTheWall daemon | Never move meeting data or consent policy into Murmur |
| Local HTTP Whisper and insertion | Murmur | Local-only skips credential resolution; original-field insertion remains app-owned |
| Local in-process Whisper and bounded queues | fotw-stt local adapter | Optional runtime and capability extraction still pending |

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
moving branch or silently update to upstream main. An upstream compatibility job
will test source changes against an immutable consumer revision before migration
is considered complete.

Audio/STT vendor removal, provider capabilities, the shared audio/transcription
conformance suite and complete upgrade compatibility gates remain outstanding.
Shared Rust source does not share loaded model memory. Sharing model workers is
separate #120 work, conditional on measured contention.

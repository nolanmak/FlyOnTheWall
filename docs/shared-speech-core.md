# Shared speech contracts (#119)

FlyOnTheWall maintains the reusable audio, transcription and credential crates.
This increment exposes opt-in `conformance` features for downstream tests and
separates the native Whisper runtime from the transcription contracts.

| Contract | Maintained owner | Application responsibility |
| --- | --- | --- |
| Credential addressing, secret wrappers, redaction, OS key store | `fotw-secrets` | Environment/config precedence and whether a provider needs credentials |
| Audio format, callback and clock types; platform taps | `fotw-audio` | Consent, recovery policy, deadlines and generation fencing |
| Transcript segments, provider normalization and local-stream lifecycle | `fotw-stt` | Retries, persistence, fallback and delivery |
| PCM resampling | `fotw-dsp` | Each app owns capture, channel selection and resampler lifetime |
| Meeting WAL and recovery | FlyOnTheWall daemon | Never moved into Murmur |
| Dictation insertion and final drain | Murmur | Remain application-owned |

## Features and behavioral checks

`fotw-stt` consumers must explicitly enable `local-whisper` to compile the
macOS whisper.cpp/Metal implementation. `fotwd` enables it, preserving the
recording app's local transcription. Consumers using an external local helper
can import the same types without building another model runtime.

The `conformance` features are intended for consumer dev-dependencies. They
exercise synthetic credential storage/redaction, audio framing and clock units,
transcript timing and final/interim semantics, replay bounds, silence, local
end-of-stream drain, queue overload and abandoned-worker output fencing.
They use no real credentials, microphone, cloud service or model weights.
The producer runs these contracts in its own tests as well.

## Compatibility and upgrade policy

`.github/workflows/murmur-compatibility.yml` checks immutable Murmur migration
commit `85df901f00a5d68e55466f0717cf68694373b48b` on macOS and Linux. It replaces
that consumer's four Git dependencies with the exact producer crates under
review and runs its workspace tests. Cargo may update source entries in this
disposable checkout; ordinary consumer builds retain their committed lockfile.
This is a pinned migration consumer, not a claim that Murmur main has already
removed its vendor snapshot or that every later consumer revision is compatible.

For a consumer upgrade, review a full producer commit SHA, update dependency
and dev-dependency pins together with Cargo.lock, and run both repositories'
required checks. Roll back those manifest/lockfile changes together. Never use
a moving branch as the dependency version. Advance the compatibility consumer
pin deliberately as migrations ship. Preserve both repositories' Apache-2.0
LICENSE/NOTICE files.

Complete capability negotiation, consumer vendor removal and newer
consumer coverage remain tracked in #119. Shared source does not share loaded
model memory: a shared inference worker remains separate #120 work, conditional
on measured contention.

## Shared PCM processing

`fotw-dsp` contains the existing pipeline resampler and downmixer unchanged.
`fotw-pipeline::resample` re-exports the same public types, preserving existing
callers. The new crate depends only on rubato and thiserror; importing it does
not import the pipeline's Opus codec, meeting storage or native capture.

The DSP tests cover supported rates, stereo channel order and anti-aliasing.
Its `conformance::streaming()` also checks identical output across arbitrary
callback splits (including split stereo frames), filter phase continuity,
finite samples, output duration and PCM encoding across six input rates.
The pinned consumer compatibility job now substitutes all four shared crates.

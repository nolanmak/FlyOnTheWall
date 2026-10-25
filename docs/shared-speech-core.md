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

`.github/workflows/murmur-compatibility.yml` checks immutable Murmur consumer
commit `bac36507b9871bc8ecd9b4286cf4ce02d9dba464` (Murmur #81) on macOS and Linux. It replaces
that consumer's four Git dependencies with the exact producer crates under
review and runs its workspace tests. Cargo may update source entries in this
disposable checkout; ordinary consumer builds retain their committed lockfile.
This is the merged Murmur consumer with its vendor snapshot removed, native
readiness/cleanup ownership, whole-utterance fallback, vocabulary and opt-in text
cleanup. Both its Deepgram CloseStream/Metadata drain and external local helper
remain application-owned. Future consumer revisions require another deliberate
pin update; passing this job is evidence for this exact consumer only.

For a consumer upgrade, review a full producer commit SHA, update dependency
and dev-dependency pins together with Cargo.lock, and run both repositories'
required checks. Roll back those manifest/lockfile changes together. Never use
a moving branch as the dependency version. Advance the compatibility consumer
pin deliberately as migrations ship. Preserve both repositories' Apache-2.0
LICENSE/NOTICE files.

Provider capabilities are explicit in `fotw-stt::SttCapabilities` (streaming,
batch, timestamps, vocabulary, diarization and language detection), with runtime
language validation in the Whisper adapter. Murmur now consumes these contracts
without keeping vendored copies. Its ordinary locked CI checks the producer pin
in its manifest; this workflow checks the producer under review against the
current pinned consumer, including the consumer-owned transport regressions.
Shared source does not share loaded model memory: a shared inference worker remains separate #120 work, conditional
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

## Local language validation

The in-process Whisper adapter validates language codes against the installed
runtime before opening a model. It then checks `WhisperContext::is_multilingual`
from model metadata; file names are not evidence of language support. An
English-only model accepts English and resolves automatic selection to English;
other explicit languages produce a non-retryable Unsupported error. The local
setup command loads and validates the selected model/language before saving the
configuration. Session workers repeat validation when creating a recognizer.
Meeting entry points now prepare native model states before capture or provider
connection, and the dashboard shows model/language and live/saved source
capabilities. Preparation timeout retains ownership until actual cleanup.
Physical-device and paired model-quality acceptance remain separate release gates.

Validation: the new normalization/capability test failed before implementation;
local adapter unit tests and settings regressions pass after the change. The
ignored `real_english_model_rejects_non_english_from_metadata` test was also run
with the installed base.en artifact (SHA-256
`a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002`), under the name
`model.bin`, and passed. It reads model metadata without capturing or uploading
audio. Normal CI requires no weights; rerun the explicit model test with
`FOTW_WHISPER_ENGLISH_MODEL` pointing to an English-only model and
`cargo test -p fotw-stt --features local-whisper --lib real_english_model -- --ignored`.

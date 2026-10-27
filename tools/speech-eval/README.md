# Paired raw-ASR evaluation

This is the paired quality runner for #118 and Murmur #9. It runs explicitly
configured, pinned CLI adapters against one checksummed audio snapshot. It never
downloads models/audio, starts capture, reads a product library or queries Keychain.

`paired_execution_complete` means every candidate completed every fixture/trial.
It requires at least two candidates. Missing/skipped/failed work makes the paired
comparison null and the command exits 1. Individual successful-trial scores remain
available, with their denominators. This is **not release qualification**: human
corpus/licensing review, warm release-to-result latency, memory/swap, sustained
load, streaming backlog and physical-app acceptance remain separate #118 gates.
The report lists those unmeasured items. No candidate/default is selected.

## Run

Python 3.9+; standard library only. Use trusted adapters whose code/dependencies
match the supplied pins. Paths are explicit; no model discovery or provisioning.

```sh
python3 tools/speech-eval/run_benchmark.py hash-asset /absolute/model-or-bundle
python3 tools/speech-eval/run_benchmark.py run \
  --corpus /absolute/corpus.json --candidates /absolute/candidates.json \
  --repetitions 5 --output /absolute/new-report.json
```

`run` skips cloud candidates before reading a credential or starting an adapter.
`run-cloud` is the separate upload opt-in. It permits configured cloud adapters
with `DEEPGRAM_API_KEY` supplied in the environment; missing credentials are an
explicit skip. It never retrieves keys from the OS credential store. Neither
command inherits unrelated provider variables, HOME configuration or stdin.

Local execution requires macOS `/usr/bin/sandbox-exec`, using a deny-network
profile. Unsupported isolation is a skip, never a bare-network local run. Linux
runs the model-free tests/scorer; this runner does not claim Linux offline
execution. Child process groups have deadlines and shared stdout/stderr limits;
owned descendants are terminated even after a successful parent exits. Adapters
must stay in that owned process group.

Reports are private, atomically created and never overwrite an earlier run. They
omit transcript/reference contents, argv, artifact paths and credential values.
No run outputs/audio belong in Git. Exit codes: 0 complete pairing, 1 incomplete
pairing, 2 invalid configuration or IO failure.

## Corpus schema v1

A manifest has exactly `schema_version: 1` and `fixtures: [...]`. Each fixture:

```json
{
  "id": "utterance-1",
  "audio": "fixtures/utterance-1.wav",
  "sha256": "<64 lowercase hex digits>",
  "reference": "C++ uses 20 threads",
  "terms": ["C++"],
  "kind": "human",
  "track": "utterance",
  "tags": ["technical", "numbers"],
  "license": "<documented redistribution rights>",
  "source_url": "https://<public-source>",
  "source_revision": "<immutable source revision>"
}
```

Audio is mono PCM16 16kHz WAV, at most 30 minutes/60MB per fixture, within the
manifest directory. Hashes, actual PCM duration and provenance fields are checked
before execution. The corpus snapshot is limited to 2 GiB. `kind` is `human`,
`synthetic` or `silence`; silence has an empty reference. `track` is `utterance`,
`mic` or `system`. Meeting tracks may share a `meeting_id`. Coverage counts tracks
as a ten-minute pair only when exactly one mic/system pair has matching durations
and both are at least ten minutes. Repetitions never inflate fixture counts.
Kind/rights annotations are supplied by the operator, not independently verified.

Human, synthetic and silence results are separate, with per-tag subsets. Scoring
uses NFC and casefolding. Technical identifiers and numeric signs, decimals and
currency symbols are retained.
WER is word-level edit distance; zero-reference WER is null, with hallucinated
words counted separately. Term recall counts exact normalized phrase occurrences;
it is not a semantic-meaning score. Numeric errors score the numeric-token
sequence. Alignments above 12 million token-pair cells are an explicit scoring
limit, not a completed comparison or a protocol failure. Raw ASR is evaluated
independently of cleanup/diarization.

## Candidate schema v1

The candidate manifest has `schema_version: 1` and `candidates: [...]` (1–8).
For the existing production Whisper helper:

```json
{
  "id": "whisper-small-en",
  "mode": "local",
  "format": "fotw-whisper-smoke-v1",
  "executable": "/absolute/local_whisper_smoke",
  "executable_sha256": "<binary SHA-256>",
  "runtime_revision": "<source revision and runtime version>",
  "model_id": "whisper-small.en",
  "model_path": "/absolute/ggml-small.en.bin",
  "model_sha256": "<model SHA-256>",
  "timeout_seconds": 180
}
```

Build it explicitly with `cargo build --locked --release -p fotw-stt --example
local_whisper_smoke --features local-whisper`. Its fixed invocation uses a derived
float32 PCM copy and one leg. The existing helper limits input to 120 seconds;
it does not establish long-meeting streaming/backlog acceptance. Its reported
model path and duration use the existing production protocol; the executable and
model are pinned before invocation.

Generic adapters use `format: "asr-eval-v1"` and an `argv` list of arguments after
the pinned executable. Supported literal substitutions are `{audio_wav}`,
`{audio_f32}`, `{model}`, `{model_id}`, `{terms_json}`. There is no shell expansion.
`use_terms: true` passes annotated terms; otherwise the terms list is empty.
Whisper smoke cannot claim unsupported keyterms. Declare baselines and vocabulary
variants as separate candidate IDs.

A generic adapter must emit one bounded JSON object:

```json
{
  "schema_version": 1,
  "audio_sha256": "<actual input WAV SHA-256>",
  "model_id": "<requested model ID>",
  "text": "<raw ASR result>"
}
```

Optional `segments` require `timestamp_unit: "ms"`, integer in-bounds
`start_ms`/`end_ms`, and text consistent with the transcript. A missing transcript,
wrong source/model receipt or malformed output is a failure, never silence.

Directory models are supported. Their fingerprint is SHA-256 of compact ASCII
JSON `["speech-eval-tree-v1", [[relative_POSIX_path, file_SHA256], ...]]`, sorted by
path. Empty bundles, symlinks/non-files, over 10,000 files, 20,000 total entries, 32 directory levels or 16 GiB are
refused. Single-file pins use the ordinary file SHA-256. Optional `runtime_assets`
is a list of `{ "id": "adapter-script", "path": "/absolute/adapter.py",
"sha256": "<fingerprint>" }` for external adapter scripts/frameworks/lockfiles.
File identity/size/change metadata is checked before and after each trial; changed
artifacts invalidate the pair without rehashing large weights for every request.
Pin all relevant runtime artifacts; a source revision alone does not freeze an
untracked Python environment. Model/binary/runtime pins are checked before running.

Cloud adapters use `mode: "cloud"`, generic format and
`credential_env: "DEEPGRAM_API_KEY"`; local model paths/pins are omitted. A candidate
not provisioned yet is explicitly `{ "id": "parakeet", "mode": "local",
"unconfigured": true }`. It cannot claim invented model/runtime pins.

### Nova-3 adapter

`deepgram_adapter.py` supplies the prerecorded English Nova-3 candidate. It uploads
only the explicit WAV, once, to `https://api.deepgram.com/v1/listen`. Its CLI also
requires `--allow-cloud`; the runner's `run` command still skips it before launch.
There is no endpoint override, proxy discovery, redirect following, retry or model
discovery. Credentials come only from the explicitly supplied environment.

Use an **actual available dated version** from your provider configuration, with
model ID `nova-3@YYYY-MM-DD.REVISION`. `latest` and bare `nova-3` are rejected. The
request sets that exact `version`, and the response must identify one model with
matching version and `nova-3` architecture before it becomes an ASR result. This
pins the provider's reported version, not a remotely verifiable weight hash. No
version availability or live provider success is implied by synthetic tests.

Configure a generic cloud candidate as above, with these fields:

```json
{
  "id": "nova3-raw-baseline",
  "mode": "cloud",
  "format": "asr-eval-v1",
  "executable": "/absolute/pinned/python3",
  "executable_sha256": "<Python binary SHA-256>",
  "runtime_revision": "<adapter Git revision and Python version>",
  "model_id": "nova-3@<actual dated version>",
  "credential_env": "DEEPGRAM_API_KEY",
  "use_terms": false,
  "timeout_seconds": 180,
  "argv": ["/absolute/deepgram_adapter.py", "--allow-cloud",
           "--audio", "{audio_wav}", "--model-id", "{model_id}",
           "--terms-json", "{terms_json}"],
  "runtime_assets": [
    {"id": "adapter", "path": "/absolute/deepgram_adapter.py", "sha256": "<SHA-256>"},
    {"id": "protocol", "path": "/absolute/benchmark.py", "sha256": "<SHA-256>"},
    {"id": "scoring", "path": "/absolute/speech_eval.py", "sha256": "<SHA-256>"}
  ]
}
```

Resolve the executable symlink before fingerprinting, and pin the Python runtime
environment. The adjacent protocol/scoring modules are runtime dependencies.
Create a second candidate with a distinct ID and `use_terms: true` for a vocabulary
trial. The adapter sends repeated, URL-encoded `keyterm` parameters (up to 100 plain
terms), preserving phrases and symbols. It does not use the legacy weighted
`keywords` option. English, punctuation, smart formatting, diarization and utterance
settings are fixed: English is enabled and the four transformations are disabled.
Thus results evaluate raw prerecorded ASR, not production streaming, endpointing,
speaker attribution, cleanup or formatted-number behavior.

`mip_opt_out=true` is always sent; Deepgram documents pricing impacts for this
option. Uploading/paid runs require separate authorization and supplied credentials.
No live request or credential lookup runs in CI. The response is byte-bounded;
errors emit a fixed diagnostic without server bodies, keys or transcripts. The
socket timeout is 120 seconds; the runner's owned process deadline supplies the
total wall-time bound, including a server that sends data slowly.

Protocol references: [prerecorded audio](https://developers.deepgram.com/reference/speech-to-text/listen-pre-recorded),
[version](https://developers.deepgram.com/docs/version),
[keyterm prompting](https://developers.deepgram.com/docs/keyterm).

### Parakeet Unified adapter (macOS)

`parakeet/` supplies the local English Unified 0.6B **int8** candidate used by
[Muesli](https://github.com/Muesli-HQ/muesli) at source revision
`55c207859bcf6e7c33d99568c4f731662d4c08cd`. This is not the TDT v3 candidate.
It uses FluidAudio **0.17.3**, revision
`a6b826ce9a09028fc685a4b32a8927ec920c3188`, through `UnifiedAsrManager`'s explicit
local-directory loader. The runtime performs its offline windowing; the adapter
does no capture, model discovery, download, resampling, cleanup or speaker labeling.
It rejects vocabulary hints rather than claiming an unsupported vocabulary trial.

The dependency-free checks require macOS 14+ and Swift 6.0+ (Command Line Tools
are sufficient; XCTest is not required):

```sh
env -u FOTW_FLUIDAUDIO_SOURCE swift run \
  --package-path tools/speech-eval/parakeet -j 3 parakeet-protocol-check
```

Building the actual adapter requires **Swift 6.2+** and a separately provisioned
local FluidAudio source directory at the revision above. Verify that directory
before building:

```sh
python3 tools/speech-eval/parakeet/verify_runtime.py /absolute/FluidAudio
FOTW_FLUIDAUDIO_SOURCE=/absolute/FluidAudio swift build \
  --package-path tools/speech-eval/parakeet -c release -j 3 --product parakeet-eval
```

Verification hashes all 387 selected files across Sources, both manifests and
LICENSE; it downloads nothing. A source-only directory works: upstream Tests,
audio fixtures and model weights are not needed to compile this product. Packaged
static TTS lexicon/config files under Sources are included in the source pin.
The opt-in manifest disables the NeMo text-normalizer trait. SwiftPM may still
resolve/cache the upstream checksum-pinned binary artifact at build time, even
though it is not linked. Compilation is not an offline-inference measurement.

Separately provision the model files from
[`FluidInference/parakeet-unified-en-0.6b-coreml`](https://huggingface.co/FluidInference/parakeet-unified-en-0.6b-coreml/tree/d32e972dd4315f1dc3f6be28fb2aab0ab3e80358),
revision `d32e972dd4315f1dc3f6be28fb2aab0ab3e80358`, preserving these bundles:
`parakeet_unified_encoder_int8.mlmodelc`, `parakeet_unified_decoder.mlmodelc`,
`parakeet_unified_joint_decision_single_step.mlmodelc`, plus `vocab.json` and
`metadata.json`. Each compiled bundle needs its coremldata.bin, model.mil and
weights/weight.bin files. The preflight checks layout and file types, not model
authenticity; the runner's fingerprint pins the complete operator-provisioned
directory. No weights or audio are included in this repository.

Configure a generic local candidate with `model_id:
"parakeet-unified-en-0.6b-int8"`, `use_terms: false`, the resolved release executable,
its SHA-256, the model directory and its `hash-asset` fingerprint. Set `argv` to:

```json
["--audio-wav", "{audio_wav}", "--model-directory", "{model}",
 "--model-id", "{model_id}", "--terms-json", "{terms_json}"]
```

Record the source revision and Swift version in `runtime_revision`; pin any
additional distributed runtime assets. Run through the paired runner to enforce
network denial, input/model/binary pins, process ownership and deadlines. The
adapter emits raw text only in its result JSON; its own failures use a fixed
diagnostic. Upstream/CoreML diagnostics may still appear on stderr, which the
runner bounds and discards. CI validates model-free behavior only. A successful
compile does not establish inference success, accuracy, latency, memory usage,
ANE placement or a production fallback for either app.

## Measurement scope

Candidate order rotates across fixtures/trials. Every candidate uses the same
snapshot; mutation invalidates the pair and prevents further inference. Raw
responses are scored and discarded. Failed trials are listed, not folded into an
optimistic complete comparison.

Latency is **fresh-process wall time**, including process/guard/startup/teardown,
with nearest-rank p50/p95, counts and groups by actual audio duration. It is not
warm release-to-paste latency or a claim of cold OS caches. Reported native
load/inference/total times remain separately labelled. Fresh-process real-time
factor uses this wall time divided by actual audio duration. Host OS/architecture,
CPU/RAM where available and a load-average snapshot accompany the report; they do
not establish a controlled workload or thermal/memory qualification.

There are at most 10 repetitions and 10,000 requests. Per-request deadlines are
bounded by the remaining execution-phase budget (default one hour after artifact
verification). `just ci` and both hosted test platforms run the model/provider-free
unit suite. Real-model runs require separately provisioned, approved assets.

# fotw-speechd protocol, version 1 (#243)

`fotw-speechd` is the one helper process that runs FluidAudio's Core ML models
(Parakeet, Silero VAD, and later the streaming and diarization engines) for
FlyOnTheWall and Murmur. Both apps are Rust and stay Swift-free: they spawn the
helper, talk to it over this protocol, and send it PCM. The helper never opens
the microphone or any capture device.

The Rust side is `crates/fotw-speechd-client`. It contains the frame codec and
message types, the client that owns the helper process, and
`fotw-speechd-fake`, a model-free helper used by the ordinary test suite.

## Transport

The parent starts the helper as a child process and owns its lifetime:

```
fotw-speechd serve --models-root <absolute dir> [--fake-engines]
```

- **stdin** carries frames from the parent to the helper.
- **stdout** carries frames from the helper to the parent. The helper moves its
  own stdout to stderr before loading any framework, so a library that prints
  cannot corrupt the stream.
- **stderr** is diagnostics only. It never carries transcript text.
- EOF on stdin means shut down: the helper finishes nothing, cancels every
  stream and exits 0.

## Frames

Every message is one frame:

```
u32 big-endian  L        length of everything after this field, 1 ≤ L ≤ 1 MiB
u8              kind     0x01 control, 0x02 audio
L-1 bytes       payload
```

| kind | payload | limit |
|---|---|---|
| `0x01` control | one UTF-8 JSON object with a string `type` | 64 KiB |
| `0x02` audio | `u32` big-endian stream id, then signed 16-bit little-endian mono PCM | 1 MiB frame; ≥ 1 sample; even byte count |

A frame that breaks any of these rules is a **protocol error**, and so is a
control message that is not an object, has an unknown `type`, or is missing a
required field. A protocol error is fatal to the connection, in both
directions:

- the helper writes `error {code: "protocol", fatal: true}` and exits with
  status 65;
- the client kills the helper, fails every open stream and pending request with
  a protocol error, and restarts it under the restart policy below.

A length is checked before its payload is read, so an oversized frame is
refused without allocating it.

## Handshake

The parent speaks first. Nothing else is accepted before it.

```json
→ {"type":"hello","id":1,"protocol":1,"client":"fotwd/0.1.0"}
← {"type":"hello","id":1,"protocol":1,"helper":"fotw-speechd/0.1.0","capabilities":{…}}
```

If the helper does not speak the parent's version it answers
`error {id, code: "version_mismatch", supported: [1]}` and exits 0. If the
helper answers with a different `protocol`, the client refuses it. Either way
the client reports `VersionMismatch` and does **not** retry: a restart cannot
fix a mismatched install.

`capabilities`:

```json
{
  "max_frame_bytes": 1048576,
  "max_streams": 8,
  "engines": [
    {
      "id": "parakeet-unified",
      "kind": "batch",            // batch | streaming | vad | diarization
      "partials": false,
      "languages": ["en"],
      "sample_rates": [16000],
      "model": {
        "id": "parakeet-unified",
        "repository": "FluidInference/parakeet-unified-en-0.6b-coreml",
        "revision": "d32e972dd4315f1dc3f6be28fb2aab0ab3e80358",
        "license": "CC-BY-4.0",
        "installed": true
      }
    }
  ]
}
```

## Requests and responses

Every request except `audio`, `finish` and `cancel` carries a client-chosen
`id` (u64). Its response carries the same `id`; a failed request is answered
with `error {id, code, message}` instead.

| request | response | notes |
|---|---|---|
| `hello {protocol, client}` | `hello {protocol, helper, capabilities}` | first frame only |
| `load {engine}` | `loaded {engine, model, revision, elapsed_ms}` | local files only; refuses `model_missing` / `model_invalid` |
| `warmup {engine}` | `warmed {engine, elapsed_ms}` | runs one silent chunk so the first real request pays no Core ML compile |
| `unload {engine}` | `unloaded {engine}` | |
| `open_stream {stream, engine, sample_rate, format, channels, options}` | `stream_opened {stream}` | `format` is `"s16le"`, `channels` 1 |
| `health {}` | `health {uptime_ms, streams, loaded, rss_bytes}` | |
| `install {model, from?}` | `progress {model, bytes, total}`…, then `installed {model, revision}` | **explicit setup only**; see Models |
| `shutdown {}` | `bye {}` | the helper then exits 0 |

Stream messages carry the stream id instead of a request id:

| parent → helper | meaning |
|---|---|
| audio frame | more PCM for that stream, in order |
| `finish {stream}` | no more audio; flush and close |
| `cancel {stream}` | drop the stream now; no further events for it |

| helper → parent | meaning |
|---|---|
| `partial {stream, text, start_ms, end_ms}` | provisional text; display only, never persisted |
| `final {stream, text, start_ms, end_ms, segments, model}` | committed text. A streaming engine sends one per utterance; a batch engine one per `finish`. `segments` is `[{start_ms, end_ms, text?, speaker?}]`, which is also how diarization reports speakers |
| `vad {stream, event, sample, probability}` | `event` is `speech_start`, `speech_end` or (with `options.probabilities`) `probability` |
| `error {stream, code, message}` | terminal for that stream only |
| `closed {stream, reason}` | terminal; `reason` is `finished` or `cancelled` |

Times are milliseconds and samples are counted from the stream's first sample.
After `closed` or a stream `error` the helper sends nothing more for that
stream, and the client drops anything that arrives for a stream it has
cancelled. Cancelling one stream never touches another.

Error codes: `version_mismatch`, `protocol`, `bad_request`, `unknown_engine`,
`model_missing`, `model_invalid`, `not_loaded`, `stream_exists`,
`unknown_stream`, `unsupported_format`, `too_long`, `busy`, `engine`,
`install_failed`. The client adds `helper_crashed`, `helper_unresponsive`,
`helper_failed` (restart budget spent) and `timeout`.

## Engines

| id | kind | model | license |
|---|---|---|---|
| `parakeet-unified` | batch | `FluidInference/parakeet-unified-en-0.6b-coreml` | CC-BY-4.0 |
| `silero-vad` | vad | `FluidInference/silero-vad-coreml` | MIT |
| `parakeet-eou-320ms` | streaming (planned, #245) | `FluidInference/parakeet-realtime-eou-120m-coreml` | NVIDIA Open Model License |
| `nemotron-3.5` | streaming (planned, Murmur #100) | `FluidInference/Nemotron-3.5-ASR-Streaming-Multilingual-0.6b-CoreML` | OpenMDW-1.1 |

A batch stream buffers its audio, bounded at 10 minutes, and transcribes it on
`finish`. Feed it VAD-bounded utterances (#244) or post-meeting windows (#246).
A vad stream answers as audio arrives, in 256 ms steps.

With `--fake-engines` the helper serves only model-free engines, which the
ordinary test suites use:

| id | behaviour |
|---|---|
| `fake-batch` | on `finish`, `final {text: "samples=<n>"}` |
| `fake-stream` | a `partial {text: "samples=<n so far>"}` per audio frame; `final` on `finish` |
| `fake-vad` | `speech_start` at the first sample with \|x\| ≥ 16384, `speech_end` at the next all-zero frame |
| `fake-crash` | the process exits with status 70 on its first audio frame |
| `fake-hang` | the process stops reading and answering on its first audio frame |
| `fake-stall` | never answers `finish`, but keeps serving everything else |

## Models

Each model is pinned to a repository, a full revision and the size and SHA-256
of every file (`helpers/fotw-speechd/Sources/SpeechdCore/PinnedModels.swift`).
Weights are never committed. They live under
`<models-root>/<model id>/<revision>/`, next to a completion marker.

- **Only `install` touches the network**, and only the parent sends it, from an
  explicit setup action (settings, `fotwd stt models install …`), never during
  capture. `install {model, from: "<dir>"}` imports from a local directory
  instead and opens no socket.
- An install downloads into a staging directory, checks every file's size and
  hash, and only then moves the revision into place and selects it. A file that
  fails validation fails the install and leaves the previous selection as it
  was.
- `load` reads only local files and checks every file's size against the pin.
  It never downloads or repairs.

## Lifecycle owned by the client

- **Spawn and restart.** The client spawns the helper, and after a crash
  restarts it with bounded backoff: at most 3 attempts in a row, each at least
  2 s after the previous spawn. A stream that completes resets the count. When
  the budget is spent the client reports `helper_failed` until the app resets
  it. After a restart it replays every `load` and `warmup` the app asked for.
- **Crash.** When the helper exits or its stdout closes, every open stream gets
  `helper_crashed` at once and every pending request fails. Nothing waits on a
  dead helper.
- **Hang.** While any stream is open the client sends `health` every 5 s. A
  request that misses its deadline (10 s for `health`), or audio the helper
  stops reading for 2 s, marks the helper unresponsive: the client kills it and
  handles it as a crash, so a recording never blocks on transcription. The
  helper must therefore answer `health` while an engine is busy. A stream with
  no result 120 s after `finish` means a wedged engine in a helper that still
  answers, and is handled the same way.

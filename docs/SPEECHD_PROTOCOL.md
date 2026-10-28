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

**The helper must never block reading or writing frames on inference.** It
keeps reading stdin and answering `health`, `cancel`, `open_stream` and other
streams' audio while any engine is busy, however long that takes (a
10-minute batch at a real-time factor of 0.1 is a minute of inference). It
queues a stream's audio for that stream's engine, bounded (the Swift helper
fails a stream with `busy` past 2 minutes of backlog). The client kills a
helper that leaves its stdin unread for 2 s or a heartbeat unanswered for
2 s, so a helper that blocked would be killed mid-meeting. `fake-batch` takes
2.5 s to finish to hold both helpers to this.

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
control message that is not an object or is missing a required field, and a
*request* of an unknown `type`. A protocol error is fatal to the connection,
in both directions:

- the helper writes `error {code: "protocol", fatal: true}` and exits with
  status 65;
- the client kills the helper, fails every open stream and pending request with
  a protocol error, and restarts it under the restart policy below.

A length is checked before its payload is read, so an oversized frame is
refused without allocating it.

**Forward compatibility.** Additions within version 1 are new optional fields,
new reply or event types, and new engine kinds. Both sides ignore fields they
do not know. The client skips a reply of an unknown `type` that has no `id`,
fails just that request if it has one, and leaves engines of an unknown
`kind` out of the capabilities it reports. Requests cannot grow new types
within a version: a helper refuses one as a protocol error.

## Handshake

The parent speaks first. Nothing else is accepted before it.

```json
→ {"type":"hello","id":0,"protocol":1,"client":"fotwd/0.1.0"}
← {"type":"hello","id":0,"protocol":1,"helper":"fotw-speechd/0.1.0","capabilities":{…}}
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
      "max_audio_ms": 600000,     // per stream; absent means unbounded
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
| `load {engine}` | `progress {phase: "load"}`…, then `loaded {engine, model, revision, elapsed_ms}` | local files only; refuses `model_missing` / `model_invalid`. A first load on a machine includes Core ML's compile, tens of seconds |
| `warmup {engine}` | `progress {phase: "warmup"}`…, then `warmed {engine, elapsed_ms}` | runs one silent chunk so the first real request pays no Core ML compile |
| `unload {engine}` | `unloaded {engine}` | |
| `open_stream {stream, engine, sample_rate, format, channels, options}` | `stream_opened {stream}` | `format` is `"s16le"`, `channels` 1 |
| `health {}` | `health {uptime_ms, streams, loaded, rss_bytes}` | |
| `install {model, from?}` | `progress {phase: "download" \| "verify", model, bytes, total}`…, then `installed {model, revision}` | **explicit setup only**; see Models |
| `shutdown {}` | `bye {}` | the helper then exits 0 |

Stream messages carry the stream id instead of a request id:

| parent → helper | meaning |
|---|---|
| audio frame | more PCM for that stream, in order |
| `finish {stream}` | no more audio; flush and close |
| `cancel {stream}` | drop the stream now; no further events for it |

| helper → parent | meaning |
|---|---|
| `partial {stream, text, start_ms, end_ms, utterance?}` | provisional text; display only, never persisted |
| `final {stream, text, start_ms, end_ms, segments, model, utterance?}` | committed text. A streaming engine sends one per utterance; a batch engine one per `finish`. `segments` is `[{start_ms, end_ms, text?, speaker?}]`, which is also how diarization reports speakers |
| `vad {stream, event, sample, probability}` | `event` is `speech_start`, `speech_end` or (with `options.probabilities`) `probability` |
| `error {stream, code, message}` | terminal for that stream only |
| `closed {stream, reason}` | terminal; `reason` is `finished` or `cancelled` |

`utterance` numbers a streaming engine's utterances from 1 within a stream. A
later `partial` with the same `utterance` replaces the earlier one in place,
and the `final` with it commits it (live captions, #245). `progress
{id, phase, model?, bytes?, total?}` reports a slow request's phase:
`download`, `verify`, `load` or `warmup`; unknown phases are allowed.

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

Each engine states its per-stream bound as `max_audio_ms` in the
capabilities, and a stream past it fails with `too_long`.

- A **batch** stream buffers its audio, at most 10 minutes for
  `parakeet-unified`, and transcribes it on `finish`. Feed it VAD-bounded
  utterances (#244) or post-meeting windows (#246: 30 s with 2 s overlap).
- A **vad** stream answers as audio arrives, in 256 ms steps. Unbounded.
- A **streaming** stream sends `partial`s as audio arrives and a `final` per
  utterance, with `utterance` ids. Unbounded; state is the engine's cache.
- A **diarization** stream (#246) takes a whole channel of a meeting, up to
  `max_audio_ms` (4 hours for the planned `diarizer`). It processes audio as
  it arrives in bounded windows, as Muesli's `diarizeRecordedAudio` does, and
  keeps per-window speaker embeddings rather than the audio, so memory does
  not grow with the meeting. On `finish` it sends one `final` with empty
  `text` and `segments` `[{start_ms, end_ms, speaker}]` sorted by start,
  covering the stream's speech; speaker labels (`"S1"`, `"S2"`, …) are stable
  within that stream only. The 10-minute batch bound does not apply to it.

With `--fake-engines` the helper serves only model-free engines, which the
ordinary test suites use:

| id | behaviour |
|---|---|
| `fake-batch` | on `finish`, after 2.5 s, `final {text: "samples=<n>"}`, while still serving everything else |
| `fake-stream` | a `partial {text: "samples=<n so far>", utterance: 1}` per audio frame; `final {utterance: 1}` on `finish` |
| `fake-vad` | `speech_start` at the first sample with \|x\| ≥ 16384, `speech_end` at the next all-zero frame |
| `fake-crash` | the process exits with status 70 on its first audio frame |
| `fake-hang` | the process stops reading and answering on its first audio frame |
| `fake-stall` | never answers `finish`, but keeps serving everything else |
| `fake-exit-after-final` | on `finish`, `final` and `closed`, then the process exits with status 70 |

`load` and `warmup` of a fake engine send one `progress` each.

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
- **Shut down.** `shutdown` returns promptly even mid-restart: the restart
  stops at whatever step it is on (spacing, spawn and handshake, or replayed
  loads) and the helper it started is killed.
- **Crash.** When the helper exits, the client reads what it wrote before
  dying (a last `final`, its own fatal `error`) to the end of its output,
  for at most 250 ms, then fails every open stream with `helper_crashed` and
  every pending request. A crash reaches open streams within 1 s; nothing
  waits on a dead helper.
- **Hang.** While any stream is open the client sends `health` every 1 s with
  a 2 s deadline, so a hung helper is found within 3 s by default, even after
  `finish` when no audio flows. A queue to the helper that stays full for 2 s
  means the same. Either way the client kills it and handles it as a crash, so
  a recording never blocks on transcription. A stream with no result 120 s
  after `finish` means a wedged engine in a helper that still answers, and is
  handled the same way.
- **Sending audio.** `send_audio` waits at most 2 s for room in the queue to
  the helper (then the helper is treated as hung). Call it from a task, never
  a real-time capture callback; there, `try_send_audio` queues one frame
  without waiting and returns `Backpressure` when full, leaving the choice to
  drop or buffer to the caller.

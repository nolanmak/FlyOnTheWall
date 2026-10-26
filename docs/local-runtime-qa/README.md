# Local-engine contention on the target Mac

This opt-in suite compares the actual FlyOnTheWall `WhisperEngine` (two
recognizers sharing one model context) with Murmur's actual local HTTP adapter
and its independently managed whisper.cpp server. It uses the existing pinned
small.en model and public JFK clip. No capture, library, provider credentials,
insertion, installed-app launch, or downloads are involved.

## What the measurement says

Both independent engines completed this small fixture with external networking
denied. Concurrent inference increases dictation latency even though these
processes' sampled RSS fits comfortably within the target's 16 GiB RAM.

The retained run used Mac16,10 / macOS 26.4. Each condition ran three times in
rotated order. Every output matched the complete 22-word normalized reference:
nine Murmur outputs and twelve FOTW recognizer outputs. The two FOTW recognizers
receive the same 11-second clip; this does not evaluate speaker identification.

| Condition | Murmur request median (range), seconds | FOTW two-recognizer inference median (range), seconds | Largest sampled summed RSS, MiB |
| --- | ---: | ---: | ---: |
| Murmur only | 0.369 (0.363–0.405) | — | 741.9 |
| Murmur; FOTW loaded, idle | 0.371 (0.362–0.380) | — | 1500.1 |
| FOTW; Murmur loaded, idle | — | 0.552 (0.537–0.559) | 1409.7 |
| Both infer | 0.771 (0.765–0.778) | 0.799 (0.796–0.803) | 1424.0 |

Murmur's median increased from approximately 0.369 s alone to 0.771 s during
concurrent inference (about 2.1×). FOTW's median increased from 0.552 s to
0.799 s. Three trials are preliminary evidence, not a p95 estimate or a
statistical performance guarantee. The loaded-but-idle condition separates
additional model residency from active inference contention.

The [raw report](m4-independent-engines.json) records all trials, hardware,
source commit and binary/model/audio/helper hashes. It retains no transcripts.
The example's source is identical in the measured source commit and the report
commit; binaries are operator-built and identified by hash. Source hashes alone
cannot establish binary correspondence.

## Reproduce

Build the FOTW example from this revision and Murmur's example from merged
[Murmur #86](https://github.com/nolanmak/Murmur/pull/86), using separate worktrees.
The measurement used FOTW release mode (`whisper-rs` 0.16.0 / `whisper-rs-sys`
0.15.0 with Metal), Murmur debug mode, and the installed whisper.cpp 1.9.1 server.
Use the existing small.en model and JFK WAV from whisper.cpp v1.9.1; the runner
rejects different sizes/hashes before starting a runtime.

```sh
cargo build --locked --release -p fotw-stt --features local-whisper \
  --example local_whisper_smoke
# Separately, in the Murmur worktree:
# cargo build --locked --example local_transcribe
python3 scripts/qa-local-contention.py \
  --model /absolute/path/to/ggml-small.en.bin \
  --wav /absolute/path/to/jfk.wav \
  --fotw /absolute/path/to/fotw/target/release/examples/local_whisper_smoke \
  --murmur /absolute/path/to/murmur/target/debug/examples/local_transcribe \
  --murmur-source /absolute/path/to/murmur \
  --server /absolute/path/to/whisper-server \
  --output /absolute/path/to/report.json
```

For the retained run, the Murmur helper source snapshot is from source
`5e8157e1d315d4dbb34c6e10bee20be9ae8e88fc` (merged tree
`27af0621845a8ffeffb8c84214b30d543943fdef`). Helper hashes are in the report.
The operator built both examples before measurement; the runner checks all
three executed binaries are unchanged afterward. Run without concurrent builds.

The controller imports the model into temporary private state, starts Murmur's
existing managed gateway, and keeps that runtime loaded across all conditions.
For FOTW conditions, it starts a fresh example process and waits until its model
and two recognizers are loaded. Only then does it release inference. It closes
stdin without releasing inference for the idle condition, verifying an error
exit and no inference output. All owned child processes are waited for or killed
on failure; the helper stops through its authenticated control interface.
Temporary state is removed only after runtime control metadata is gone. A failed
shutdown retains control state for recovery.

## Start barrier and test-first evidence

`local_whisper_smoke MODEL mono16k.f32le LEGS [--wait-for-start]` preserves the
existing default behavior. The optional mode flushes one JSON readiness line
and waits for the single ASCII byte `1` on stdin. Readiness means the model and
recognizer states have been created, not that an inference warm-up has run.
EOF or another byte exits with an error. The QA controller owns readiness and
completion timeouts; this explicit waiting mode is not a production service.
The example's total duration includes time at the barrier; comparisons use its
separately reported `inference_seconds`.

Before implementation, the built example rejected `--wait-for-start` with its
usage error before reading the nonexistent test paths. After implementation,
the native [protocol check](m4-start-gate.json) observed readiness followed by
EOF/error/no inference output, then readiness/release and two complete reference
matches. The 12-condition matrix also exercises three EOF/no-inference cases
and three overlapping request intervals. This is a QA protocol change; it does
not change production speech scheduling or cancellation behavior.

## Interpretation and limits

- Murmur timing includes client process startup, HTTP readiness probe and
  inference. FOTW timing covers its two concurrent native recognizers after
  readiness. These different timing scopes must not be ranked as equivalent
  engine benchmarks.
- Concurrent requests are dispatched within approximately 0.1 ms; the report
  verifies overlapping request lifetimes. It does not profile exact GPU kernel
  overlap. FOTW uses its existing thread/default inference settings; the Murmur
  server uses its automatic GPU defaults.
- Memory is sampled from registered owned PIDs only: gateway, server, FOTW
  example and Murmur client. Polling sleeps 50 ms plus `ps` overhead. FOTW model
  loading is included; Murmur startup memory is excluded. Summed RSS double
  counts shared pages and is not unique physical memory or separately measured
  GPU allocation. Peaks are sampled, so short spikes can be missed.
- Cache state, desktop background load and thermal state are uncontrolled.
  Model import/hash verification precedes runtime startup. This is neither a
  cold-start nor a whole-app memory measurement. Real devices, app UI/storage,
  sustained meeting chunks, backlog, cancellation and larger models remain
  separate acceptance gates.
- The source clip is approved public **human speech**, not synthetic speech.
  Its f32 derivative exists only in temporary state. One clean clip cannot
  establish accents, terminology, names, noise, or paired Deepgram quality.

## Shared-worker decision

Retain independent runtimes for the current small.en path while backend quality
selection and sustained-load acceptance remain open. These results do not
justify making a new shared daemon a prerequisite for offline correctness.
There is measurable inference contention; sharing weights alone would not prove
that scheduling improves. A shared implementation would also need a versioned,
private IPC boundary, independent cancellation/lifecycle, bounded queues and
meeting/dictation fairness, plus an actual comparison against this baseline.

This advances #118/#120, without closing either. No shared-worker implementation
or memory saving is claimed. Full M4 signed-app/device acceptance and the rest
of #124 remain required.

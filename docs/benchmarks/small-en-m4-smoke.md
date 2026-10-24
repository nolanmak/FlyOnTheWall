# Local small.en smoke check on an M4 Mac mini

Recorded 2026-10-24 on an Apple M4 with 16 GiB RAM. Cached public `small.en`
weights transcribed upstream's 11-second public JFK clip through the CLI,
Murmur's actual local HTTP client, and FOTW's actual Rust recognizer. FOTW also
ran two concurrent recognizer states sharing one model. This is a resource
baseline; the paired quality/resource benchmark in #118 remains open.

## Measurements

| Path | Process wall seconds | Peak process RSS (GiB) |
| --- | ---: | ---: |
| Homebrew CLI metal-1 | 0.65 | 0.798 |
| Homebrew CLI metal-2 | 0.58 | 0.796 |
| Homebrew CLI metal-3 | 0.58 | 0.796 |
| Homebrew CLI cpu-1 | 2.73 | 0.786 |
| FOTW Rust backend, 1 leg(s) | 1.06 | 0.668 |
| FOTW Rust backend, 2 leg(s) | 1.08 | 0.760 |

Every leg contained the same 11-second clip. Fresh processes include model load
and teardown; filesystem caches and background desktop load were not controlled.
Final FOTW measurements ran after build/lint jobs ended. Its internal timer was
0.604 s for one leg and 0.732 s for two; process wall time includes later cleanup.

The CLI uses Homebrew whisper.cpp 1.9.1, four threads and default beam/best-of
settings. Metal selection was present in its runtime log. FOTW uses locked
whisper-rs 0.16.0 / whisper-rs-sys 0.15.0 with the production Metal feature,
greedy best-of-1 and up to eight threads per recognizer. These rows are not a
decoder speed ranking. RSS is process memory, not whole-system unified-memory
usage. System swap remained 2188.19 MB throughout these short checks.

Murmur's real `local_stt::transcribe` returned the expected passage from an owned
whisper-server on a temporary loopback port. Readiness took 0.535 s; the Rust
example invocation took 0.979 s including startup and any incremental Cargo
overhead, not isolated HTTP latency. The server returns segment newlines: the
initial raw-phrase harness assertion failed across a newline while the app
command itself succeeded. Whitespace-normalized verification of its captured
output passed. The owned server stopped and temporary model/state files were
removed. This does not exercise native microphone capture or insertion.

## Reproduce

Model: 487,614,201-byte `ggml-small.en.bin`, SHA-256:

```
c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d
```

Public fixture: [whisper.cpp v1.9.1 samples/jfk.wav](https://raw.githubusercontent.com/ggml-org/whisper.cpp/v1.9.1/samples/jfk.wav),
mono PCM16 at 16 kHz, 352,078 bytes, SHA-256:

```
59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e
```

Set `WHISPER_MODEL` to that model path. These commands read only the supplied
model and public fixture:

```sh
/usr/bin/time -l whisper-cli -m "$WHISPER_MODEL" -f jfk.wav -t 4 -nt
/usr/bin/time -l whisper-cli -m "$WHISPER_MODEL" -f jfk.wav -t 4 -nt -ng
python3 - <<'PY'
import struct, wave
from pathlib import Path
with wave.open('jfk.wav') as wav:
    assert (wav.getnchannels(), wav.getframerate(), wav.getsampwidth()) == (1, 16000, 2)
    values = [v[0] / 32768 for v in struct.iter_unpack('<h', wav.readframes(wav.getnframes()))]
Path('jfk.f32le').write_bytes(struct.pack('<%df' % len(values), *values))
PY
cargo build -p fotw-stt --features local-whisper --example local_whisper_smoke
/usr/bin/time -l target/debug/examples/local_whisper_smoke "$WHISPER_MODEL" jfk.f32le 1
/usr/bin/time -l target/debug/examples/local_whisper_smoke "$WHISPER_MODEL" jfk.f32le 2
```

The opt-in tool bounds input to 120 seconds, rejects malformed/non-finite or
out-of-range samples, and accepts one or two legs. Ordinary CI does not load a
model, capture a microphone or call a paid provider.

## Remaining acceptance

Short clean English speech establishes neither general WER/term recall nor a
comparison against Nova-3 or Parakeet. These checks do not cover capture/VAD,
insertion, AEC/double-talk, failover, long-meeting drift, thermal behavior or
simultaneous independent apps. Sharing a model between FOTW legs does not share
memory with Murmur. #118, conditional shared-worker work in #120 and physical
signed-app acceptance remain open.

[Machine-readable observations](small-en-m4-smoke.json) retain transcripts and
per-run values. No user audio, dictated text or provider credentials were used.

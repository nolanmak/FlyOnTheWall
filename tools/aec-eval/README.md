# AEC signal probe

This isolated evaluation tool compares the existing production `EchoGate` with
passthrough and Sonora 0.2.0's Rust WebRTC AEC3 port, directly and through the
shared pipeline frame adapter. It does not enable an app
backend, use capture devices, fetch models, access credentials or contact speech
providers. No new speech/model assets are required. The dependency is confined
to this tool's own workspace and lockfile.

```sh
cargo test --manifest-path tools/aec-eval/Cargo.toml --locked
cargo clippy --manifest-path tools/aec-eval/Cargo.toml --all-targets --locked -- -D warnings
cargo build --release --manifest-path tools/aec-eval/Cargo.toml --locked
/usr/bin/time -l tools/aec-eval/target/release/fotw-aec-eval
```

`CARGO_TARGET_DIR` can place the build cache outside the worktree. Resource
measurement runs the already-built binary, excluding compiler memory and time.
After dependencies have been provisioned, these commands also work with
`--offline`. Never substitute a private recording for the generated fixtures.

## Method and limits

Three 20-second, deterministic 16 kHz mono cases exercise near-only, echo-only
and simultaneous near/far signals. Independent seeded, band-shaped noise has
aperiodic syllable-rate envelopes. The linear synthetic room uses delayed
reflections at 40, 70 and 97.5 ms. All processors see identical input; the gate
uses its existing 100 ms test cadence and AEC3 uses exact 10 ms frames, with
render processed before capture. Render input is immutable; its processed copy
is discarded. Additional noise suppression, AGC and enforced high-pass filtering
are disabled to isolate echo processing.

The fourth backend, `sonora-0.2.0-adapted`, receives nonuniform callback chunks
of 37, 243, 79 and 211 samples. It uses the pipeline adapter's declared 160-sample
frame and 128-sample backend delay, with 320 waiting-mic and 640 reference samples
as explicit probe budgets. Its output intervals are checked against the original
clock; the delayed tail is drained without publishing padding. A real-backend
partial-Stop test first returned only 16,000 samples for 16,159 input samples;
the adapted path now retains every sample and verifies the near-only tail values.

Metrics exclude eight seconds of adaptation. A fixed backend delay is estimated
from its independent near-only control and reused for the other cases. Near gain
is the signed least-squares projection onto the known near signal. Residual
reduction compares input echo energy with output energy after subtracting that
near projection. During overlap the residual includes near-end distortion; it
is **not** an isolated echo measurement. A per-sample -120 dBFS floor bounds
perfect suppression results. Muting everything cannot pass near-preservation
controls even though its echo-only reduction looks impressive.

Near SNR uses the retained projection's power, including the measured gain,
relative to the residual. Using the louder original near signal would overstate
quality when both speech and interference are attenuated. A calibration test
first observed 6.02 dB where equal retained signal/noise powers require 0 dB;
it now verifies the corrected result.

The direct controls remain fixed-frame, undrained probes. The adapted path
compensates delay and drains the tail; per-result flags distinguish these modes.
Pipeline fake-backend tests cover independent/jittered delivery, missing and
evicted references, error/non-finite output, resets and bounded retained input.
This still does not establish physical-device timing, acoustic drift handling,
word retention or real-room quality. Equal buffer lengths alone do not establish
source-tail preservation. No app candidate is selected: overlap attenuation and
quality gates still need qualifying comparative evidence before integration.

## First M4 observation

`results/m4-fixed-frame.json` records the 2026-10-25 release run on a Mac16,10
with an Apple M4 and 16 GiB RAM. Each row processes 20 seconds of generated audio.
The already-built process used 12,173,312 bytes maximum RSS and 0.46 seconds of
user CPU across all nine cases plus metric calculation (0.62 seconds elapsed).
Those are this probe's resources, not the app or transcription model's footprint.

| Backend | Echo-only residual reduction | Overlap near gain | Overlap residual reduction | Overlap processing time |
| --- | ---: | ---: | ---: | ---: |
| Passthrough | 0.00 dB | +0.06 dB | 0.00 dB | 0.02 ms |
| Existing gate, 100 ms cadence | 94.46 dB (muted; floor limited) | -0.07 dB | 0.06 dB | 22.90 ms |
| Sonora AEC3 0.2.0 | 28.56 dB | -3.92 dB | 3.72 dB | 72.15 ms |

Near-only controls retain unity gain. Sonora's measured fixed delay is 128
samples (8 ms); the other controls measure zero. The gate removes echo-only
chunks but leaves overlap largely unchanged. Sonora cancels echo while retaining
some near signal, with material overlap attenuation/distortion that needs further
evaluation. No speech intelligibility or double-talk word retention is proven.

Before app integration, initial synthetic engineering gates are: at least 15 dB
echo-only reduction; near-only gain within 0.5 dB of passthrough; overlap near
gain within 1 dB and near SNR at least 6 dB above passthrough; plus a verified
tail drain and bounded callback/jitter/reference-failure/route-reset adapter.
These gates use the raw/gate baselines to prevent accepting echo suppression at
the expense of local speech. They are not perceptual or real-room standards.
This candidate fails both overlap gates: gain -3.92 dB and retained-near SNR
3.52 dB versus passthrough's 3.78 dB (a 0.26 dB regression). It is not selected
for the app. Native WebRTC/LocalVQE comparison,
approved speech word retention and real-room trials remain required by #72.

## M4 adapter observation

`results/m4-adapter.json` adds the adapted path while retaining the direct controls.
Its compensated output delay measures zero samples, while the direct path still
measures 128. This corrects source positions, not wall-clock latency: the adapter
still buffers callbacks and the declared backend delay. Echo-only reduction
(28.56 dB), overlap near gain (-3.92 dB) and
retained-near SNR (3.52 dB versus raw 3.78 dB) are unchanged: framing fixes do not
make an acoustically unqualified backend acceptable.

The adapted path processed each 20-second case in 62.61 ms (near-only), 84.72 ms
(echo-only) and 77.37 ms (overlap). The whole already-built twelve-case probe plus
metrics used 14,204,928 bytes maximum RSS, 0.79 seconds user CPU and 1.05 seconds
elapsed. This remains a synthetic worker probe, not app/model resource evidence.

## Pinned sources and provenance

- [Sonora 0.2.0](https://github.com/dignifiedquire/sonora/tree/a024d6ef8351add55be5e8b1d6cc35f555787660),
  BSD-3-Clause, Rust 1.91 minimum; lockfile pins the evaluated release. Its
  [public API](https://docs.rs/sonora/0.2.0/sonora/) accepts separate immutable
  render/capture input and output buffers. The tool's Rust 1.95 requirement
  matches FlyOnTheWall. See `SONORA-LICENSE` and the repository's
  `THIRD_PARTY_NOTICES.md` for dependency notices.
- [Native WebRTC binding 2.1.0](https://docs.rs/crate/webrtc-audio-processing/2.1.0):
  the existing #49 direction; the optional comparison below executes it. Its bundled
  build requires a C++ toolchain and Meson/Ninja/pkg-config, with additional
  build-cache/provisioning concerns recorded in #49. This probe is not evidence
  that the Rust port matches that binding's quality.
- [Muesli 55c207859bcf6e7c33d99568c4f731662d4c08cd](https://github.com/Muesli-HQ/muesli/tree/55c207859bcf6e7c33d99568c4f731662d4c08cd):
  `MeetingNeuralAec.swift` selects a neural processor and handles pending frames
  and reference history. `LocalVQEProcessor.swift` validates a 16 kHz/256-sample
  runtime and a separately provisioned GGUF plus native library. LocalVQE and
  DTLN are **not measured here**; their assets and licenses need separate
  provisioning/review. No Muesli implementation or model was copied.

The probe and fixtures are written for this repository. Existing gate behavior
is called through the maintained pipeline crate, without copying its detector.
The observations advance #72/#118/#124; they do not complete their criteria.

## Optional native WebRTC comparison

`--features native-webrtc` adds the pinned `webrtc-audio-processing` 2.1.0
binding with bundled C++ WebRTC AudioProcessing 2.1. It is confined to this
evaluation workspace and is disabled by default. The shipped app and root
lockfile do not gain a native dependency. Upstream's Rust wrapper and WebRTC use
BSD-3-Clause; Abseil uses Apache-2.0. Additional bundled notices and the WebRTC
patent grant are retained in [NATIVE-NOTICES.md](NATIVE-NOTICES.md).

Provision Rust 1.95 with `rustup component add llvm-tools-preview`, a C++17
toolchain/libclang, Python, Meson 1.11.1 and Ninja. On the measured Mac, Meson
1.11.1 and Ninja 1.13.2 were already present; LLVM tools were added to the existing
toolchain. CI provisions Meson 1.11.1/Ninja 1.13.0 in an isolated virtualenv and
checks both macOS and Linux. The upstream build invokes Ninja itself, so CI and
the local probe wrap Ninja with `-j3` as well as setting `CARGO_BUILD_JOBS=3`.

```sh
cargo test --manifest-path tools/aec-eval/Cargo.toml --locked --features native-webrtc
cargo clippy --manifest-path tools/aec-eval/Cargo.toml --locked --features native-webrtc --all-targets -- -D warnings
cargo build --release --manifest-path tools/aec-eval/Cargo.toml --locked --features native-webrtc
/usr/bin/time -l tools/aec-eval/target/release/fotw-aec-eval
```

First builds fetch public build dependencies: the Cargo lockfile pins the Rust
crates, and upstream's Meson wrap pins Abseil 20240722.0 source and patch archives
by SHA-256. No speech/model assets are fetched. `cargo --offline` does not govern
Meson's downloader: offline rebuilds require preserving the native Cargo target
and Meson source/package cache, not just the Cargo registry. A clean native
build without those archives needs provisioning. Ordinary evaluation remains
pure Rust and does not require these tools. The measured native binary links
only macOS system libraries/frameworks; no Homebrew native library is needed at
runtime. The native libraries are statically linked into this evaluation binary.

The native control uses 16 kHz mono, 160 samples per 10 ms frame, as reported by
the pinned runtime. It analyzes the immutable render reference before each
capture frame, with full AEC3, automatic delay estimation and no NS/AGC/high-pass
filter. It deliberately refuses partial frames instead of silently truncating
them. Its output has a measured 128-sample delay; this fixed-frame control does
not drain the source tail or claim callback/route/reset qualification. Those
limitations remain explicit in each result's flags.

`results/m4-native.json` records one 2026-10-25 Apple M4 run over the same
fixtures, adaptation window, metrics and engineering gates defined above:

| Backend | Echo-only reduction | Overlap near gain | Overlap retained-near SNR | Overlap processing time / 20s |
| --- | ---: | ---: | ---: | ---: |
| Passthrough | 0.00 dB | +0.06 dB | 3.78 dB | 0.01 ms |
| Sonora 0.2.0 direct | 28.56 dB | -3.92 dB | 3.52 dB | 71.88 ms |
| Native WebRTC 2.1.0 | 19.88 dB | -3.81 dB | 3.69 dB | 271.94 ms |

Both AEC implementations preserve near-only unity gain. Native WebRTC passes
the 15 dB echo-only gate but fails the same overlap requirements: near gain must
stay within 1 dB, and retained-near SNR must improve by 6 dB over passthrough.
It is **not selected for the app**. No gate was relaxed to accept this backend.
The probe test first failed with 0 dB echo reduction, then passed after actual
native render-before-capture processing was connected; its separate near-only
control prevents muting everything from passing that test.

The already-built fifteen-case probe plus metrics used 20,021,248 bytes maximum
RSS, 1.52 seconds user CPU and 1.77 seconds elapsed. The binary was 1,976,800
bytes. Rust used its release profile; upstream Meson used `debugoptimized`
(`-O2`), not an assumed matching Rust optimization setting. These are whole
probe observations, not isolated backend memory or app/transcription-model
resource estimates. Synthetic signals cannot establish word retention or
real-room quality. LocalVQE/model provisioning, approved speech comparisons,
drift/route qualification and physical trials remain open under #72/#118/#124.

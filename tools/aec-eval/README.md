# AEC signal probe

This isolated evaluation tool compares the existing production `EchoGate` with
passthrough and Sonora 0.2.0's Rust WebRTC AEC3 port. It does not enable an app
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

This is a fixed-frame signal probe. It does not flush backend delay, establish
source-tail preservation, handle arbitrary callback chunks, jitter, drift,
reference loss or route changes, measure words retained or validate real rooms.
Equal input/output buffer lengths do not establish complete audio duration when
the backend delays samples. The result explicitly says that no app candidate
has been selected. Sonora's delay and overlap attenuation must be addressed or
accepted with stronger comparative evidence before product integration.

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

## Pinned sources and provenance

- [Sonora 0.2.0](https://github.com/dignifiedquire/sonora/tree/a024d6ef8351add55be5e8b1d6cc35f555787660),
  BSD-3-Clause, Rust 1.91 minimum; lockfile pins the evaluated release. Its
  [public API](https://docs.rs/sonora/0.2.0/sonora/) accepts separate immutable
  render/capture input and output buffers. The tool's Rust 1.95 requirement
  matches FlyOnTheWall. See `SONORA-LICENSE` and the repository's
  `THIRD_PARTY_NOTICES.md` for dependency notices.
- [Native WebRTC binding 2.1.0](https://docs.rs/crate/webrtc-audio-processing/2.1.0):
  the existing #49 direction; it is not executed by this probe. Its bundled
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

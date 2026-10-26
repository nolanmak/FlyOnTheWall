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

## Opt-in public speech retention probe

On macOS, `speech-retention` adds the existing maintained `fotw-stt`
WhisperEngine. It never fetches weights or audio. Supply the already provisioned
public whisper.cpp JFK WAV and small.en weights explicitly:

```sh
cargo build --release --manifest-path tools/aec-eval/Cargo.toml --locked --features speech-retention,native-webrtc
/usr/bin/time -l tools/aec-eval/target/release/fotw-aec-eval --speech-retention /path/to/approved-jfk.wav /path/to/ggml-small.en.bin
```

The probe checks exact file lengths and SHA-256 identities before audio decode
or model construction. The accepted WAV is 352,078 bytes, SHA-256
`59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`;
the accepted model is 487,614,201 bytes, SHA-256
`c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d`.
Model hashing uses a bounded read buffer. Nothing writes to either input file.
The speech feature uses the same whisper-rs 0.16.0 / whisper.cpp 1.8.3 bindings
as the app. Their complete license texts are retained in the repository's
[third-party notices](../../THIRD_PARTY_NOTICES.md#whispercpp-and-ggml); include
those notices along with this tool's native notices if distributing this build.

The 11-second public-domain JFK excerpt is the near voice; independent seeded
generated audio is the far-end reference through the existing simulated linear
room. This is not a two-human-speaker or real-room corpus. Eight seconds of
far-only adaptation precede the speech. A one-second postroll lets each measured
fixed-delay control expose the entire original speech window; near-only sample
error and the exact 176,000-sample crop are checked before inference. This
window check does not qualify arbitrary callback tails or route resets.

Echo power is calibrated over the speech window to equal near-speech RMS and
to exceed it by 6 dB. One common gain applies to clean speech, references and all
mixtures, with input peak checks; there is no clipping or per-backend output
normalization. The model is loaded once and cases decode serially with separate
recognizer states. Actual app endpointing, streaming, diarization and insertion
are outside this probe.

Results include recognized public text, reference-word count, matches,
substitutions, deletions, insertions, word error rate and aligned reference-word
recall. Normalization splits on non-alphanumeric characters and lowercases;
equal-cost alignments prefer diagonal, deletion, then insertion. Insertions
count toward WER, which may exceed 100%; muted output cannot score as retained
speech. An actual muted-input control must not recognize more than a quarter of
the reference words. No prompt or preceding transcript is given to Whisper.

CI compiles/tests this optional path and the word metric without obtaining any
weights or speech assets. Native inference is an explicit local measurement.
The existing synthetic overlap gates remain unchanged: a good result on this
single public sentence cannot override a failed gate, select an app backend or
establish quality across accents, names, noise and physical devices.

### M4 public speech observation

[Recorded results](results/m4-public-speech.json) contain 13 serial cases using
the approved small.en model on an Apple M4. All near-only controls recognized
the 22 reference words without errors. Delay-aligned near-only samples differed
by less than `8e-8`; the full speech window remained present.

| Control | Equal echo/near power WER | Echo 6 dB above near WER |
| --- | ---: | ---: |
| Passthrough | 0.00% | 68.18% |
| Existing gate, 100 ms cadence | 95.45% | 331.82% |
| Sonora 0.2.0, adapted | 0.00% | 59.09% |
| Native WebRTC 2.1.0, fixed-frame control | 0.00% | 63.64% |

The gate's strong-echo output included a repetition hallucination: insertions
make WER exceed 100%, not a claim that more than all source words were deleted.
The actual muted-input control recognized only "you": WER 95.45%, one matched
word out of 22. That coincidental match is not evidence of retained speech.

Both AEC paths retained the sentence at equal power, where passthrough also
worked. All paths performed poorly in the stronger-echo case. These results
support the overlap concern but do not establish a generally useful AEC backend
or a production error rate for the gate. The signal-level overlap failures from
the earlier comparisons still stand; neither candidate is selected for the app.

The final already-built process used 779,190,272 bytes maximum RSS, 2.96 seconds
user CPU, 0.40 seconds system CPU and 7.69 seconds elapsed across all cases,
model hashing/loading and metrics. Recorded model load was 175.84 ms; file,
GPU and compiler-cache state was uncontrolled, so this is not a cold-load or
cold/warm acceptance measurement. These are whole-probe resources, including
the shared Whisper context, not isolated model/backend or concurrent-app memory.

## Conservative native profile and validation matrix

The optional native comparison now includes
`native-webrtc-2.1.0-conservative`, using the same pinned 2.1.0 runtime's
experimental AEC3 configuration API. It changes two things:

- Scale normal and near-end low/high-band echo-to-near transparency/suppression
  thresholds by 16, so nonlinear suppression is less aggressive toward mixed
  local speech. Echo-to-masker thresholds and NS/AGC/high-pass settings stay as
  before; there is no output gain normalization.
- Extend refined/coarse and initial adaptive filters to 32 blocks of 64 samples,
  covering 128 ms at 16 kHz. That is filter span, not processing latency: the
  measured fixed near-only signal offset remains 128 samples, or 8 ms.

The runtime must accept the configuration without silently modifying it. The
profile parameters are included in both generated-signal and public-speech
JSON. A real-native regression first measured the default profile's overlap
failure, then passed the original four gates with the configured profile.

The original fixture was used to choose these parameters. They were fixed
before evaluating three additional signal/room fixtures; no parameter was
changed after seeing the failed validation case. Run the full matrix with:

```sh
cargo run --release --manifest-path tools/aec-eval/Cargo.toml --locked --features native-webrtc -- --heldout-signals
```

Each fixture records independent seeds, room taps and drift. The drifting room
uses linear interpolation of the delayed far signal at +80 ppm while leaving
the render reference on its original clock. This is a synthetic sample-clock
challenge, not real hardware or callback scheduling. All cases retain their
original sample count and the same eight-second adaptation exclusion.

| M4 conservative profile | Echo-only reduction | Overlap near gain | SNR improvement over raw | Four signal gates |
| --- | ---: | ---: | ---: | --- |
| Original calibration room | 20.00 dB | -0.67 dB | +6.64 dB | Pass |
| Unseen shorter room/seeds | 19.61 dB | -0.27 dB | +8.20 dB | Pass |
| Unseen longer room/seeds | 18.80 dB | -0.38 dB | +6.18 dB | Pass |
| Unseen +80 ppm room/seeds | 14.55 dB | -0.32 dB | +1.54 dB | **Fail** |

The automated assessor uses the unchanged thresholds documented above and
rejects non-finite measurements. The aggregate requires every fixture and every
backend's entry; a failed or missing validation result cannot be hidden by a
passing calibration. Tests reproduce that failure before the aggregation fix.
CI executes this matrix but does not claim that every compared backend passed
acoustic qualification merely because the tool ran successfully.

The approved JFK probe now includes the conservative profile, for 16 serial
cases. Its near-only, equal-power and +6 dB cases all recognized the 22-word
reference with 0% WER. The stock native +6 dB control still measured 63.64% WER.
This is an improvement on one static-room public clip, not general quality or
drift acceptance. The pinned runtime's `has_clock_drift` path changes suppression
decisions; it does not resample or align the reference, so it was not used as a
substitute for clock correction.

[Signal matrix](results/m4-conservative-signals.json) and
[public speech results](results/m4-conservative-speech.json) retain the positive
and negative evidence. The full 72-case signal process used maximum RSS
34,340,864 bytes, 9.27 seconds user CPU and 9.50 seconds elapsed. The separate
16-case speech process, including the shared model/hash/load/metrics, used
766,885,888 bytes maximum RSS, 4.27 seconds user CPU, 0.41 seconds system CPU
and 9.52 seconds elapsed. Cache state was uncontrolled; these are whole-probe
observations, not app memory or end-to-end latency.

**The aggregate fails, and no app backend is selected.** Callback/tail/drift
alignment, capture-worker integration, route resets, LocalVQE comparison and
physical/real-room acceptance remain separate requirements. A passing static
signal or one public sentence cannot grant them.

## Observation-based clock alignment (evaluation only)

Run `cargo run --release --locked --features native-webrtc -- --clocked-signals`
from this tool directory, with the native build prerequisites above. No speech
or model asset is needed. The processor receives synthetic native clock traces,
not the generator's known drift value. The pipeline's bounded clock tracker fits
each leg and maps microphone frames to fractional reference positions. The native
profile parameters and existing quality gates are unchanged.

This probe assumes native 16kHz mono inputs and uses linear interpolation. It does
not qualify bandlimited correction of resampled 48kHz/stereo capture. Eight callback
periods of observation lead plus an acquisition/delivery budget simulate worker
backlog; they are not a measured application latency. Callback delivery jitter
does not replace native timestamps. Clocks need two seconds of evidence; missing
or invalid alignment passes original microphone samples through.

The existing AEC adapter supplies framing, delay compensation, reset fallback and
tail drain. Every output interval is checked for contiguous source positions,
unchanged total duration and bit-exact originals for raw fallback. A native
regression checks the complete near-only waveform across a timing fault and Stop,
not only an overall vector length. The measured compensated delay is zero.

The nominal/+80ppm/-120ppm cases pass the original gates, but the timing-fault
fixture fails echo and overlap quality during fallback/relearning. Aggregate
acceptance remains false and no app backend is selected. All four fixtures were
exercised during development; this is not an untouched held-out corpus. The first
draft's uncompensated reset-loop failure was fixed by using the existing adapter,
not by changing thresholds or native parameters. Public speech word retention,
physical clocks/rooms and production capture-worker/replay integration remain open.

The fixture vectors hold the complete generated inputs; this is not a production
reference-history queue. The whole 36-case M4 probe used maximum RSS 20,234,240
bytes, 6.33s user CPU, 0.01s system CPU and 6.53s elapsed. Cache state was
uncontrolled; these are whole-probe figures, not isolated backend or app latency.
The [retained report](results/m4-clock-alignment.json) includes the failed fault
case and false aggregate acceptance. A runtime regression first failed when the
reference path deliberately used equal sample indexes; it now requires the three
continuous cases to pass and the fault failure to remain visible.

## Bounded bandlimited native reference

`cargo run --release --locked --features native-webrtc -- --bandlimited-signals`
adds the bounded sinc reference path beside the prior linear control. It uses
incremental native input, a 4000-frame history at 16kHz, complete filter support,
and explicit history/AEC epoch reset on invalid timing. Old-epoch work is refused.
No microphone audio is filtered by the reference sampler; missing reference uses
the existing adapter's original-mic fallback and compensated tail policy.

Additional diagnostics test 8.5/9/15kHz alias rejection from native 44.1/48/96/192/
768kHz capture, plus native48kHz reference mapped to actual 44.1/44.16/48kHz
microphone converters with independent +80/−120ppm clocks. Stereo clock counts
are tested with already-downmixed reference PCM. Correct rational converter
origin reduces measured phase RMS error from about .025/.044 to about 1.5e-7 in
the fractional cases. The tested aliases are all below −80.8dB (gate:−65dB).
These are generated diagnostics, not physical clock or acoustic acceptance.

The [retained report](results/m4-bandlimited-reference.json) includes all four
fixture outcomes, raw fallback counts and the failed fault/relearning case.
No existing AEC parameter or quality threshold changed. Aggregate acceptance is
false. Complete generated fixture inputs still exist outside the bounded staging
component; this is not an implemented recording worker or a memory claim for it.
The whole 48-case/diagnostic M4 probe used maxRSS25,935,872bytes, user9.40s,
system.01s and elapsed9.60s with uncontrolled cache, not isolated backend/app cost.

The shared ArchiveConverter accessor reports existing phase; it does not shift
stored audio or change the existing reconstruction recipe. An eventual AEC
integration needs its own persisted processing/clock provenance and cannot reuse
the plain raw-to-canonical recipe for cleaned output. Production scheduling,
late-output fencing, source archival, speech retention and physical/room evidence
remain unfinished; no app backend is selected.

# fotw-speechd

The FluidAudio speech helper (#243). FlyOnTheWall and Murmur are Rust; the
models they want (Parakeet, Silero VAD, and later Nemotron, Parakeet realtime
EOU and the diarizer) are Swift and Core ML. This one signed executable runs
them, and the apps talk to it over [the protocol](../../docs/SPEECHD_PROTOCOL.md)
through `crates/fotw-speechd-client`. The core Rust build never needs Swift.

The helper never opens a capture device. The parent sends it PCM.

## Layout

| Path | What |
|---|---|
| `Sources/SpeechdCore` | Frames and messages, the server, the model store, lifecycle guards, the model-free diarization session and word timings, the fake engines |
| `Sources/SpeechdFluid` | The FluidAudio engines: `parakeet-unified`, `silero-vad`, `parakeet-eou-320ms`, `parakeet-eou-160ms`, `nemotron-3.5`, `diarizer` |
| `Sources/fotw-speechd` | The executable |
| `Sources/SpeechdChecks` | Model-free checks (no XCTest needed) |

`Package.swift` (Swift 6.0, Command Line Tools) builds everything except
`SpeechdFluid`. `Package@swift-6.2.swift` adds it when `FOTW_FLUIDAUDIO_SOURCE`
names a local FluidAudio checkout. Neither manifest clones anything or fetches a
model.

## Checks without models

```sh
just speechd-check
```

This runs the Swift checks (frames, Muesli's load/shutdown generation guard,
shared loads, a corrupt install keeping the previous selection, the pins, the
diarization session's windows, merge and speaker cap, word timings), then
`cargo test -p fotw-speechd-client --test protocol` against this helper in
`--fake-engines` mode. `just test` runs it on macOS.

## Building with FluidAudio

Swift 6.2 or later, and FluidAudio 0.17.3 at
`a6b826ce9a09028fc685a4b32a8927ec920c3188` checked out somewhere, then:

```sh
export FOTW_FLUIDAUDIO_SOURCE=/absolute/path/to/FluidAudio
just speechd-helper   # verifies the checkout, builds packaging/build/speechd-swift/release/fotw-speechd
```

To put it into the app bundle, set `FOTW_SPEECHD=1` for `just bundle`,
`just dev-sign` or `just release-sign`. It lands in `Contents/Helpers`, signed
before the app with the hardened runtime, its own identifier and no
entitlements, and `just verify-bundle` fails if it ever carries the
microphone entitlement:

```sh
FOTW_SPEECHD=1 just dev-sign && FOTW_SPEECHD=1 just verify-bundle
```

`speechd-helper` checks the checkout with
`tools/speech-eval/parakeet/verify_runtime.py` first. SwiftPM may still fetch
FluidAudio's checksum-pinned NeMo binary artifact while resolving; it is not
linked.

## Models

```sh
speechd=packaging/build/speechd-swift/release/fotw-speechd    # from `just speechd-helper`
"$speechd" models --models-root <abs dir>                   # pins, licenses, installed?
"$speechd" install parakeet-unified --models-root <abs dir> [--from <abs dir>]
"$speechd" verify parakeet-unified --models-root <abs dir>
```

`--models-root` and `--from` take absolute paths only; the helper refuses a
relative one rather than resolving it against wherever it was started.

Every model is pinned to a revision with each file's size and SHA-256
(`Sources/SpeechdCore/PinnedModels.swift`). Installing is the only thing that
downloads, and the apps run it only from an explicit setup action. `--from`
imports a local copy instead and opens no socket. An install that fails any
check leaves the previous selection as it was. Loading reads local files only.

| Model | License |
|---|---|
| `parakeet-unified` | CC BY 4.0 |
| `silero-vad` | MIT |
| `parakeet-eou-320ms` (live captions, #245) | NVIDIA Open Model License |
| `parakeet-eou-160ms` (live captions, faster variant, not the default) | NVIDIA Open Model License |
| `nemotron-3.5` (560 ms + 1120 ms variants; streaming dictation, Murmur #100) | OpenMDW-1.1 |
| `diarizer` (#246) | Unconfirmed (#277): the model card scopes CC-BY-4.0 to other files and lists these two as legacy exclusions (see `THIRD_PARTY_NOTICES.md`) |

## Real-model suite

```sh
PARAKEET_DIR=<hash-checked Parakeet copy> COREML_CACHE=~/Library/Caches/fotw-speechd \
  just speechd-real <models root> <corpus dir>
```

`COREML_CACHE` is deleted before the cold start, so it includes Core ML's
first compile, as on a first install.

Setup installs the two models (the only network use), then everything runs
under `sandbox-exec` with the network denied while `lsof -i` polls the helper:
cold and warm start, first request against the 5th, a sanity WER over the
corpus, VAD on speech and silence, and a check that an install fails inside the
sandbox. It prints a JSON report and exits 1 if a gate fails.

### Post-meeting pass smoke (#246)

```sh
PARAKEET_DIR=<hash-checked Parakeet copy> \
  just speechd-postpass-real <models root> <voice a.wav> <voice b.wav>
```

Installs `diarizer` (and Parakeet, from `PARAKEET_DIR` if set), mixes 180 s
of the voices from 300 s in (`OFFSET_S`, `DURATION_S`), and requires the
diarizer to find at least two speakers in sorted segments, and Parakeet's
`timings` words to match its text. The voices are 16 kHz mono WAVs, for
example two AMI headset channels of one meeting.

## Credit

Ported in part from [Muesli](https://github.com/Muesli-HQ/muesli) (MIT, © 2026
Pranav Hari) at `97e455794112126a896cae17f5c829e8ffa820c8`; each ported file
names its source. FluidAudio is Apache-2.0. See `THIRD_PARTY_NOTICES.md`.

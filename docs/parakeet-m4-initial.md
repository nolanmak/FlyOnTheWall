# Initial M4 Parakeet evaluation (#118)

Date: 2026-10-24. Hardware: Apple M4 Mac mini, 16 GiB RAM, macOS 26.4.
Runtime: FluidAudio v0.17.3, commit a6b826ce9a09028fc685a4b32a8927ec920c3188;
release fluidaudiocli built with Swift 6.3.2. Command:

```sh
.build/release/fluidaudiocli unified-benchmark --mode both --max-files 30 --write-md OUTPUT.md
```

The upstream runner selects the first 30 test-clean files sorted by filename.
This is a small human read-speech sample, not a meeting/dictation evaluation.
Rust compilation ran concurrently. Download/setup time and cold model loading
are excluded from the per-file speed table. No Nova-3 comparison, offline
network-denial proof, memory measurement, vocabulary test, or noisy/overlapping
speech evaluation is established here. Model hashes and a paired fixture manifest
remain required for the final benchmark. Do not use this run to select a default.

Below is the measured section of the generated report. Its hardcoded comparison
against TDT v3 was excluded because this run did not measure TDT v3.

# Parakeet Unified 0.6B — LibriSpeech test-clean Benchmark

Measured through the FluidAudio Swift managers (`UnifiedAsrManager` for batch,
`StreamingUnifiedAsrManager` for streaming) over all 30 `test-clean` files,
scored with the repo's `TextNormalizer` (same normalization as `asr-benchmark`).
Encoder precision: **int8**. Run with `swift run -c release fluidaudiocli unified-benchmark`.

| Mode | Avg WER | Aggregate WER | Median WER | Median RTFx | Overall RTFx | Long files (>15s) |
|------|---------|---------------|------------|-------------|--------------|-------------------|
| batch | 1.55% | 0.78% | 0.00% | 93.4x | 90.5x | 2 |
| streaming | 1.42% | 0.62% | 0.00% | 58.2x | 57.9x | 2 |

- **Avg WER** is the mean of per-file WER (matches `asr-benchmark`'s "Average WER").
- **Aggregate WER** is total errors ÷ total words across the set.
- Long files (> 15 s) are transcribed with overlapping 15 s windows merged on a 2 s overlap (batch),
  or as one continuous session (streaming) — none are skipped. Streaming's overall RTFx drops on
  long files because it re-encodes a 7.68 s window per 1.04 s chunk (the latency tax); batch only
  re-encodes the 2 s overlap, so its throughput stays flat.
- RTFx is end-to-end per file (Swift mel + encode + greedy RNNT decode) on the run machine.
  Mel features are computed natively in Swift (`AudioMelSpectrogram` + NeMo per_feature
  normalization); there is no CoreML preprocessor stage.


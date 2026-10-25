# Archive format across capture recovery

Both daemon and CLI now allocate a separate bounded ring for each tap attempt.
A replacement can deliver inside `start()`; those samples stay out of the retired
ring. At the recovery boundary the pump drains and finishes the old converter,
pads the gap in archive units, and installs a fresh converter for the replacement.
The track's original archive rate/channels remain fixed. Rate conversion uses
an anti-aliasing FFT filter, trims its latency and flushes exactly
`floor(native_frames * archive_rate / native_rate)` frames per generation.
Unchanged formats are bit-exact before the existing WAL quantization. Mono is
duplicated for a wider archive; other differing channel counts fold to their
mean, since capture does not provide speaker-position metadata. Partial
interleaved frames carry between drains. The strict converter rejects non-finite
PCM and incomplete final frames. Capture callers replace non-finite samples with
silence in place and discard only an incomplete final frame, reporting both
counts. Complete buffered FFT tails are retained. A converter backend failure
stops conversion for that generation and reports missing audio while allowing
the already-written recording to finalize. Conversion runs on the pump thread before the WAL; ASR/AEC still
run after the WAL write. Native-rate originals are not separately archived.

The daemon feeds retired tails, gap padding and converted final tails through
the same ASR path, preserving the provider's clock across recovery. Retired ring
drop counts are retained. New-generation PCM cannot be mixed with late old-ring
writes; native callback quiescence itself is still a separate requirement.

Behavioral RED/GREEN: five converter tests initially failed on identity conversion,
including 16,001 mono samples versus 96,006 required archive samples. The recorder
fixture initially drained replacement-start PCM before the gap (zero queued new
samples instead of 160). It now verifies persisted old PCM, zero gap padding,
new-format conversion through the actual pump, 10 ms of output at both archive
and ASR rates, and preservation of prior bytes. Separate tests cover fractional
44.1 kHz rates, odd stereo drains, anti-alias rejection, finalization and isolation
from retained old producers/failed attempts. These are synthetic fixtures, not
physical-device evidence. Live sample-rate notifications remain open.

Native device/format notifications, callback quiescence and physical persisted-
audio acceptance remain open. This does not archive native-rate originals
separately from the fixed-format tracks.

Capture sinks now enqueue a complete callback or drop/count that complete
callback. A short sample-level ring write could split an interleaved frame and
shift the channels of later PCM. The old sample-level ring API remains available
for non-capture users; daemon (both supervised and prebuilt) and CLI capture
sinks use the complete-block operation. The single-producer capacity check and
copy allocate nothing, acquire no locks and never retry on the audio thread.

A real sink fixture first exposed five queued samples instead of four after a
stereo overflow. It now verifies intact frames, whole-block drop counts and
correct channel order after draining and accepting another callback. Ring tests
cover wraparound, oversized/empty callbacks and allocation freedom. This preserves
sample shape; it does not recover dropped audio or add precise-position overrun
gap markers. Those timeline/backfill requirements remain open.


A review regression also filled the initial ring before creating the generation
factory. The replacement accepted zero samples instead of eight because capacity
was read from free slots. The factory now retains the backing ring’s immutable
capacity; the regression preserves all old queued PCM and a full replacement.

Review regressions exercise normal drains and retired-generation recovery in
both daemon and CLI using synthetic non-finite PCM and incomplete stereo tails.
They verify good PCM, zero gap padding, final duration, and `ended_at_ms`; daemon
warnings flow through the existing user-visible degradation channel. Converter
tests compare salvaged FFT tails with strict conversion of the complete input
prefix across identity, upsampling and downsampling.

## Complete Opus tails and page clocks

Opus finalization now pads enough audio to release the codec's delayed samples
before setting EOS. One extra packet suffices for a frame-aligned input, but a
nearly full partial frame can require two. The original input count and final
trim position stay exact. A regression first decoded only 108 of 159 input
samples at 8 kHz; forty empty/short/aligned/partial cases across all five native
Opus rates now decode to exactly their original sample count.

Ordinary page granules count decoded packet samples. Pre-skip is reflected in
the final intended PCM end; adding it to ordinary page clocks shifted their
starting position. A second regression observed a 2,232-tick page position for
1,920 decoded samples. It now checks packet durations through libopus, ordinary
page positions and enough decoded input to support the final trim. See
[RFC 7845 sections 4.2–4.5 and 7](https://www.rfc-editor.org/rfc/rfc7845.html#section-4.2)
for pre-skip, granule positions, end trimming and required encoder padding.
The implementation is written for this repository; no RFC code was copied.

All eleven Opus tests (including waveform, frequency/envelope, arbitrary pushes
and playable interrupted prefixes) and full local `just ci` pass. These tests
run the actual native codec over synthetic audio without capture, credentials,
providers or user recordings. The change prevents new archive tails from being
lost; existing archived bytes are not rewritten or repaired. Archived-input
verification and finalized-but-unindexed capture recovery remain separate #12
work.

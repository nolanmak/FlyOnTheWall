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
verification and finalized-but-unindexed recovery are described below.

## Archive conversion and retry integrity

Unsupported input rates now use the archive converter's explicit final flush.
A regression at 44.1 kHz first decoded 16,000 samples where 16,006 were required;
fixtures at 11.025, 22.05, 44.1 and 96 kHz now preserve the exact complete-frame
duration and audible tail. Each completed Opus file is synced before publishing
the completion manifest that permits raw PCM retirement.

Encoding checks both raw sources and output file types before opening either
archive. Missing or shortened previously encoded PCM, linked destinations and
an orphan microphone archive fail without replacing retained audio or its
manifest. An absent, never-recorded microphone with no archive remains valid.
On Unix both destination opens also refuse final-component symlinks at open
time, including a link introduced after preflight. Manifest publication syncs
the session directory after renaming the synced manifest, before PCM retirement.
Seven synthetic integration tests and full local `just ci` pass. Writer exclusion
and verification before retirement are described below. Transactional replacement
after an encoding I/O failure remains open.

## Recovering finalized captures and retired PCM

Startup recovery now admits an unindexed finalized capture through the same
saved first-Stop clock and canonical-source checks used for an interrupted WAL.
Already indexed sessions use their saved database clock and ordinary retry path;
startup does not repeatedly decode their archives during discovery. Existing
legacy indexed sessions without modern capture-ownership sidecars remain usable.

If a checkpointed raw track has been retired, recovery requires its completed
archive. A streaming verifier checks Ogg checksums, sequence and continuation,
the app's mono Opus headers, every packet through libopus, final EOS and timing,
and absence of trailing or chained data. It uses bounded reusable page, packet
and decoded-frame storage. Observed file length, original input rate, exact
sample count and duration must agree with the manifest and original checkpoint.
Consistent older app page clocks are accepted without rewriting stored bytes.

Retained raw evidence takes precedence: an uncheckpointed tail still prevents
inventing a capture end, even when an archive exists. Corrupt/truncated archives,
metadata mismatches, linked files and an unknown original Stop are refused.
All configured canonical sources are validated and leased before any are sealed
or projected. Capture ownership survives cancellation and native runtime cleanup.
Recovery preserves the manifest, retained audio, settings, notes and first-Stop
clock; archive audio is evidence, not reconstructed transcription input.

Synthetic contracts exercise clean and interrupted retired captures, exact
44.1 kHz frame conversion, corruption including valid-CRC timing edits, missing
canonical tails, cancellation, source contention, idempotence and legacy retry.
The opt-in native fixture uses only the checksum-pinned public JFK WAV and
cached `small.en` model. Neither fixture establishes physical-device recovery
or transcription quality. Raw-only/canonical reconstruction, unknown-end and
legacy unindexed capture admission remain open #12 requirements.

## Verification before destructive archive retirement

Raw PCM retirement now decodes and verifies both completed replacement archives
before unlinking either source. Header rate, complete Ogg/Opus framing, exact
sample duration and file length must agree with the original frame counts;
retained raw lengths must match the completed encode exactly. Missing/corrupt
replacement audio and newly appended or truncated raw samples preserve both
sources. A healthy repeated retirement remains idempotent.

The same verifier is shared by capture-clock proof and media promotion. Publishing
checks both source archives before opening media outputs. Directory retirement
verifies both published native-decoded artifacts, their channel/path/metadata
identity and retained raw counts before renaming or deleting the session. A
same-size CRC-corrupt media file no longer passes a size-only check. Existing
canonical-source completion and database-projection acknowledgements remain
required independently.

Encoding, publishing and destructive retirement retain the existing modern
capture lease, so a capture or local recovery worker prevents competing mutation.
The lease is not manufactured for legacy sessions that have no ownership sidecar.
Verification uses bounded reusable buffers and runs off the capture callback.
Tests use synthetic owned recordings; they do not establish physical power-loss
or device-recovery behavior. Transactional replacement of an existing archive
after an encoding I/O error, raw/canonical reconstruction and unknown-end capture
admission remain separate work.

## Automatic recovery of unknown-end canonical prefixes

Startup now attempts eligible saved canonical prefixes even when a capture has
no provable final Stop. Each startup controller makes one serial pass, using
only the model, revision and language in the original saved local plan. It
does not choose a new provider or download a missing model. Missing-model
preparation fails visibly and leaves a manual retry available; cancelling the
pass keeps ownership until the retained worker has actually returned.

Successful prefix recovery remains Partial: the capture end and duration stay
unknown, raw and canonical audio remain retained, and whole-input seal,
completion and projection markers are not created. A later startup reconciles
the prefix acknowledgement without repeating inference or overwriting existing
transcript edits. Raw-only captures without a canonical plan remain visible
without an inference attempt; reconstruction is still separate work.

`ui_preview --automatic-prefix` exercises the normal startup pass over synthetic
audio with fake inference. `--interrupted-prefix` intentionally indexes without
starting inference so reviewers can still exercise manual Retry. Normal app
startup always uses the automatic pass.

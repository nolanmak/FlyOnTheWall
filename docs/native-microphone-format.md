# Native microphone format validation

Microphone startup now reads the input stream's actual virtual ASBD, rather
than combining nominal sample rate with an assumed mono channel count or
falling back to 48 kHz when probing fails. It accepts a plausible integral
sample rate and native packed Float32 with consistent frame/packet sizes.
Interleaved multichannel and mono Float32 are supported; planar multichannel
and other encodings return FormatUnsupported rather than being misinterpreted.
Supporting those layouts requires a separate conversion path.

The IO callback verifies buffer count, channels and complete frame alignment
before reading samples. A mismatch emits the allocation-free
`TapError::StreamFormatChanged` and delivers no PCM. This is a transient device
fault suitable for rebuild. It does not advance the frame counter or report an
empty buffer as healthy delivery. The error introduces no platform types into
the shared seam. It does not by itself add native rate-change listeners or wire
all consumer recovery paths; those remain #121/#61 work.

Synthetic native tests first reproduced stereo reported as mono, acceptance of
invalid/unknown descriptors and changed-layout audio appended under old metadata.
The passing tests preserve stereo frame timing, reject incompatible descriptors,
and verify that rejected callbacks neither append samples nor refresh delivery
health. No hardware microphone was opened for these tests. Physical USB/Bluetooth
format changes and rebuilding after them remain native acceptance gates.

Before updating Murmur's producer pin, its capture conversion must also retain
partial stereo frames across ring-drain boundaries. FlyOnTheWall already passes
interleaved frames through the streaming resampler before downmixing; Murmur's
current downmix-before-resample path can discard a split frame. This follow-up
is required for the new multichannel format reporting, not completed by these
backend tests alone.

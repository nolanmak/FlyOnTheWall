# Worker-side echo frame adaptation

`fotw-pipeline::aec::Adapter` provides one backend-neutral framing/reference
boundary for #72. It does not choose a canceller or enable one in the capture
pump. The existing gate and app behavior are unchanged. Backend selection still
requires the comparative, speech, resource and physical acceptance in #72/#118.

The owner archives original mic/system audio first, converts worker input to
16 kHz mono, preloads an exclusive synchronous backend and supplies route epochs
and absolute sample offsets. Constructor limits and backend-reported frame/delay
metadata must agree before any native frame call. Every analysis call precedes
its paired capture call and receives separate copied frame storage. Original
render audio is never mutable backend output.

Mic and reference callbacks may arrive independently and in arbitrary chunk
sizes. A bounded reference history and waiting-mic budget pair exact intervals.
References outside the retained window or across a gap cause raw passthrough,
not microphone suppression. The budgets count samples, not elapsed wall time;
owners must size them for expected callback bursts and call
`flush_reference_wait` on their wall-clock deadline if a reference stream stalls.
Oversized callbacks cannot grow retained input beyond the configured bounds;
caller-owned emitted output and backend-owned state are measured separately.

Declared processing delay is removed from the output clock, with unpublished
original mic samples retained for failure recovery. Stop pads only backend
frames, drains the delayed tail and publishes exactly the accepted original
sample count. Padding never advances source offsets. Stop is idempotent. Route
reset releases pending originals with their old epoch/offset before clearing
history; old-generation references are rejected. Invalid calls cannot partially
advance capture state.

A backend error, partially written frame or non-finite processed output releases all unpublished original
samples, including delayed ones, and fences further backend calls until explicit
reset. Missing references do not repeatedly reset a fresh unused backend. Output
intervals retain processed/raw provenance and a degradation reason. They are not
claims that speech quality has passed acceptance. Invalid non-finite *input* is
rejected before acceptance; upstream capture conversion owns input sanitation.
Reset reports backend invocation health explicitly when the new generation is
accepted but backend reset fails; it does not report readiness for that failure.

All calls run on a worker, never on a realtime capture callback. This component
does not supply a thread scheduler, preempt a blocked backend, catch arbitrary
backend panics or replace an owner's native-work lifetime/cancellation fencing.
An integrating worker must preserve capture ownership, fence late publications
and await actual backend teardown before reusing or releasing that resource.
Capture-pump wiring, native worker teardown-race tests and real acoustic timing
are separate integration requirements, not completed by these contracts.

## Capture timing observations

Daemon and CLI capture sinks now publish a bounded latest observation alongside
each generation's PCM ring. `RingConsumer::capture_observation` makes one attempt
to read a coherent callback number, absolute accepted interleaved sample position,
accepted sample count, device frame index, host timestamp, original flags and
cumulative dropped samples and a timing-fault count. Overruns publish zero accepted samples. Empty callbacks
do not supersede an observation. Untimed ring writes also advance sample positions.
Old tap callbacks cannot publish into a replacement generation's clock mailbox.
Construction allocates the mailbox; callback publication has fixed atomic work
with no allocation, lock, retry, logging or dependency on pump progress.

This is latest-value monitoring evidence, not per-chunk timing: observations may
be superseded, may be stale, and may precede or follow an independently drained
PCM chunk. A reader must use sequence/position evidence, reject timestamp-error
or discontinuity flags, detect drops and reset on generation changes. Compare
`timing_faults` as well: it counts callbacks carrying either timing fault and
survives replacement by a later unflagged observation. Silence is not a timing
fault. `None`
means no observation yet or publication in progress, never an instruction to
spin. It is not a complete callback journal or a durable reconstruction recipe.

macOS capture now uses Core Audio input timestamps when both native sample and
host time are valid. A bracketed absolute/continuous clock reading projects
`mach_absolute_time` into the existing sleep-aware process epoch. A bridge wider
than 100 microseconds, an offset change exceeding that bound, future/stale input
(older than one second), pre-epoch/overflow values or non-increasing mapped time
cannot qualify timing. Audio passes unchanged with a monotonic callback-arrival
fallback and TIMESTAMP_ERROR. Epoch shifts and backward mapped time also flag
DISCONTINUITY. Native sample gaps/resets flag discontinuity; malformed or inexact
sample times cannot qualify timing. Delivered device-frame counts remain unchanged.
All conversion state is owned by the existing per-tap sink try-lock and replaced
at start; the conversion uses fixed arithmetic/native clock reads on the callback.

This validates timestamp handling, not acoustic alignment, device latency or
physical sleep/wake. It does not estimate device rates, resample the reference,
or enable AEC in the pump. Those remain integration requirements; the conservative
native profile's drift validation is still failing. Physical-device qualification
of timestamp availability and latency is also outstanding.

## Evidence

Initial runtime tests failed for delayed/partial-tail output, late reference
pairing and backend-error fallback. Additional failures exposed acceptance of a
wrong declared native frame shape and sixteen unnecessary resets while an unused
backend had no reference. These contracts now pass.

Fifteen synthetic integration tests cover these paths plus 90 combinations of
160/256/512-sample frames, zero/small/multi-frame delays and empty/aligned/partial
inputs; original audio nonmutation; eviction/gaps; NaN output; route generations;
atomic input refusal; Stop/deadline handling; and independent jittered delivery
over unique source samples. They verify values and provenance on the original
clock, not just vector lengths.

The existing isolated AEC tool also drives the actual pinned Sonora backend
through this adapter. Its partial-Stop test first observed 16,000 output samples
from 16,159 input samples. It now verifies all samples and the near-only tail
values, while the M4 probe measures zero compensated delay. The backend still
fails overlap-quality gates. No model asset, device, provider, credential or user
recording is used by these tests. See `tools/aec-eval/README.md` for methodology,
retained licenses and source references.

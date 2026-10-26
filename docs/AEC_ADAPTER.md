# Worker-side echo frame adaptation

## Thread ownership and bounded publication

`aec_worker::Worker` moves a preloaded `Backend + Send` and the existing adapter
onto one exclusive OS thread. Pump/control calls use bounded `try_send` admission
and non-waiting result polling; they never await a native call. This is not a
realtime callback API: owner-side copies, allocations and channel operations are
allowed. Backend construction/loading precedes worker creation.

The owner retains original microphone samples until matching output is accepted.
Commands contain at most 3200 samples and queues at most 16 blocks. Owner originals
are bounded to 16000 samples; adapter retention has its existing separate limits.
There is one in-flight command and a result queue of `queued_blocks + 1` messages.
Each result contains only the adapter's previously bounded pending output plus
one bounded command's output. Caller-drained output, native backend state and
allocator/channel overhead are separate from these sample budgets.

Source offsets remain contiguous within an external route epoch. Saturation,
expired publication deadlines, cancel and route reset publish remaining originals
and advance a private worker fence. Old queued/in-flight results cannot publish
after that fence. Deadlines use caller-supplied monotonic milliseconds and are
checked before accepting mailbox results. The owner must keep polling; a deadline
does not create an independent timer or guarantee scheduling latency.

Stop requests adapter tail draining without blocking the pump. If its command
cannot be admitted, pending originals are released instead. Cancel releases
originals immediately. Neither operation kills a blocked native call. A
metadata-only status stays running through actual backend destruction, including
unwinding; route reset reuses that exclusive worker rather than starting a
replacement beside a retained resource. A disconnected/failed worker degrades to
original audio. Backend panic-abort behavior cannot be recovered by a Rust thread.

Deterministic tests block capture and destruction separately and cover Stop,
timeout, saturation, reset, cancellation, late publication, panic, invalid input,
partial tails and sample conservation. The native evaluation test also moves the
actual pinned WebRTC backend through this worker and checks its 128-sample delay
compensation and partial Stop tail with generated near-only audio.

This worker accepts already aligned mono 16 kHz reference/mic inputs. It does not
yet connect the recording pump, map native/canonical conversion phases, archive
audio, or persist processed replay provenance. Production integration must write
originals before submission, bind timing/route ownership, preserve every emitted
interval's processing provenance and retain the resource status until teardown.
The raw reconstruction recipe must not be reused for cleaned audio. No acoustic
quality threshold or backend-selection decision changes here.

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
fallback and TIMESTAMP_ERROR. Epoch shifts and reversals of qualified native host time also flag
DISCONTINUITY. Native sample gaps/resets flag discontinuity; malformed or inexact
sample times cannot qualify timing. Delivered device-frame counts remain unchanged.
An otherwise ordered native time that lags a previous arrival-time fallback is
not evidence of interrupted audio: it remains a timing error without inventing
a discontinuity. This matters for Murmur, which cancels on source interruption.
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

## Clock-rate fitting and evaluation

`reference_clock::Tracker` fits native mono-frame positions against qualified
host timestamps on a worker. It retains at most 64 observations, samples at most
once per 100ms, prunes beyond eight seconds, and needs at least eight points over
two seconds. A prototype estimate requires a rate within 500ppm of nominal and
maximum residual 100us. These are admission limits, not physical qualification.

Future/stale snapshots (older than 1s), timing flags (silence is allowed), changed
fault/drop counters, inconsistent positions or callback sequences and callbacks
longer than 1s cannot qualify a fit. Consecutive callback positions must match the
prior complete callback; skipped observations must account for accepted device
frames without splitting channels. Repeated snapshots cannot create duration or
refresh evidence. New generations reset fitting; old generations cannot enter it.

`Estimate::map_frame_from` checks freshness of both streams and maps only captured
frame extents. Integer epochs/counters remain separate from relative floating
point calculations, including counters above 2^53. The owner must invalidate cached
estimates after a fault/drop/generation change and reconstruct the tracker for a
format change. Estimates are monitoring models, not a timestamp journal or an
acoustic delay measurement. Their positions are native mono frames: applying them
after nominal resampling requires explicit position/filter-delay conversion.

The isolated native tool now exercises this geometry with synthetic clock traces,
linear reference interpolation and the existing bounded AEC adapter. Rate fitting
never receives the fixture's ppm. The adapter preserves source duration, compensates
native delay, drains tails and releases original samples on timing failure. A real
native near-only regression first failed with 128 samples of uncompensated delay;
it now verifies every sample across warmup, a timing fault, recovery and Stop.

Continuous nominal/+80ppm/-120ppm signal cases pass the unchanged gates. The
timing-fault case still fails aggregate echo/overlap quality; original fallback
samples are retained exactly. Linear interpolation is an evaluation control,
not a qualified bandlimited resampler. Production pump/worker wiring, mapping
through capture conversion, reference-quality acceptance, replay provenance,
speech/physical/real-room qualification and backend selection remain unfinished.

## Native-rate bandlimited reference staging

`reference_resample::ReferenceBuffer` now supplies bounded native-mono reference
history and bandlimited lookup into the microphone's 16kHz processing clock. The
owner downmixes complete native frames before append. Typed native/processing
positions, host timestamps, tap generations and alignment epochs keep the units
explicit. Both clocks must share the same capture-host epoch.

The preloaded normalized Blackman-windowed sinc uses 257 phase rows, interpolated
between adjacent phases, and 64 taps per ceiling(native-rate/16kHz). Its cutoff is
90% of the smaller input/output Nyquist. Support grows with native rate through
768kHz. History is capped at two seconds, appends at one second, and requests at
3200 processing frames; the owner configures a smaller working history as needed.
The filter and fixed scratch buffers allocate only at construction. Full support,
clock validity and all positions are checked before any caller output is changed.
Missing edges are refused, not zero-padded into purported reference audio.

`ArchiveConverter::output_origin` exposes the pinned FFT kernel's rational native
offset after integer output-delay trim, without changing PCM, duration or the
persisted v1 reconstruction recipe. At 44.1kHz→16kHz the residual is +1/2 native
frame; at 44.16kHz→16kHz it is −22/25. The reference mapping uses this signed offset.
For native→archive→processing conversion, compose the actual stage origins:
`phase_native = phase_capture + phase_processing * native_rate / archive_rate`.
For example the direct and two-stage 44.16kHz conversions have different origins.
Tests compare against actual converter output, including two-stage paths, rather
than assuming that trimming an integer delay establishes a zero-phase origin.

Timing failure advances the alignment epoch and clears the retained reference and
AEC state. Late writes/requests from the old epoch are rejected even when native
taps did not restart. Render format changes require a newly preloaded filter;
microphone origin/rate and tap bindings can change on an explicit reset. This is
worker work despite allocation freedom, not work for the capture callback.

The native evaluation now drives this buffer with incremental captured slices,
retaining at most 4000 native frames at 16kHz instead of using the whole fixture
as reference history. Original fallback, compensated tails and contiguous source
intervals are still checked by the existing AEC adapter. Measured continuous
nominal/+80ppm/−120ppm cases pass existing signal gates; the fault/recovery case
still fails aggregate echo/overlap quality and no app backend is selected.

Actual recording-pump/worker lifetime, cancellation/backpressure, raw-first
archival and processed-replay provenance remain integration requirements. The
shared converter metadata is additive; existing consumers and PCM output are
unchanged. Device/room and speech-word qualification remain separate gates.

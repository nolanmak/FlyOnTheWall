# Device recovery status (#121)

The shared capture supervisor retains its ineffective-rebuild count across brief
returns of audio. A ten-second run of frame and buffer progress, with audible
evidence since the last watchdog arm and no watchdog fault, resets the count.
The same confirmation releases quarantine while allowing incoming audio to
continue reaching the recorder. Empty callbacks, a poll without progress,
counter rollback, and observations separated by the starvation timeout restart
confirmation. Rebuilding or rearming also discards the previous confirmation.
This uses the injected capture clock; tests do not sleep or open audio devices.

Behavioral regression tests in `crates/fotw-audio/tests/watchdog.rs` first exposed
both immediate quarantine release and ten rebuilds from ten one-second bursts.
They now verify the configured ceiling, the ten-second boundary, interrupted
confirmation, and renewed recovery after sustained delivery. Run:

```sh
cargo test -p fotw-audio --test watchdog
just ci
```

Recovery attempts now share one episode across failed opens, successful starts
that do not restore sustained health, and route changes. The default is three
attempts, at least fifteen seconds apart; failed opens use capped exponential
backoff. Exhaustion cannot be reset by route chatter. `request_retry()` queues
an explicit new episode while preserving the original outage and spacing floor.
Stop or taking the tap cancels a queued retry and detaches the old notification
mailbox; reused supervisors need a new device watcher. Synthetic tests exposed
five default retries, a reset after brief success, and 167 open calls (including
initial startup) under route chatter before the fix. They verify mixed failures, successful route rebuilds,
manual retry/gap boundaries, and stopped-session notification isolation.

The legacy `Recovered` event remains the immediate format/gap boundary for
writers. Its user message now says restart is being confirmed; a separate
`DeliveryConfirmed` event fires once after sustained health. The supervisor also
exposes bounded, content-free delivery snapshots. A session-owned monitor carries
independent system/microphone state to the recording-status API and dashboard.
The dashboard keeps controls mounted across polls and distinguishes checking,
healthy, silent, recovering, exhausted and unavailable sources. Retry uses the
normal authenticated ingress and requires an exact active session ID plus an
exhausted source. Stop closes the monitor, discards pending requests and refuses
late publications; old browser responses cannot restore stopped/replaced state.

Tests cover source independence through the real pump adapter, authenticated
routing, stale session rejection, Stop, retry deduplication, preserved controls,
and late browser responses. A pending Stop immediately removes retry controls;
status polls cannot restore them until the stop request finishes.
`ui_preview --capture-recovery` offers a synthetic
browser fixture with a healthy system source and exhausted microphone. Retrying
moves it through checking to healthy without opening devices or providers.

Explicit device-invalidated, device-not-found and stream-format callback errors
now latch their first timestamp in a per-attempt atomic mailbox. Further PCM
from that faulted sink is refused. The next control poll enters recovery without
waiting for starvation; retries retain the existing episode budget and spacing.
Repeated errors cannot move the outage start, and an old sink's mailbox cannot
fault a replacement. An exhausted callback fault requires explicit retry; unlike
watchdog quarantine it cannot confirm health by accepting more suspect PCM.
The eventual recovery gap uses `callback-fault` and preserves the first fault
time even across exhaustion and manual retry.

`crates/fotw-audio/tests/callback_recovery.rs` first failed because explicit
callback faults still returned Healthy. Three synthetic tests cover prompt
rebuild, post-fault PCM refusal, old-error isolation, attempt spacing/ceiling,
Stop, and complete outage timestamps after manual retry. These checks do not
prove native teardown or callback quiescence. The wrapper observes reported
faults; it does not create missing native sample-rate notifications or implement
format conversion.

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
interleaved frames carry between drains; an incomplete final frame or non-finite
PCM is refused. Conversion runs on the pump thread before the WAL; ASR/AEC still
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

Each supervised attempt also owns an atomic callback admission flag. Retiring a
tap revokes new frame/error delivery before native Stop; detach for asynchronous
teardown and supervisor drop revoke it too. Failed startup revokes its retained
sink, and every fresh attempt gets a new flag. This prevents retired callbacks
from updating shared watchdog/session counters, reporting stale errors or adding
PCM after the attempt ends. It applies even to a healthy tap retired by watchdog
recovery, not only one that explicitly reported a fault.

Two behavioral regressions first demonstrated retired frames reaching the sink
and Stop accepting a second sample. Seven callback tests now cover faults,
budgets/gaps, retirement, Stop/detach/drop, failed initial/recovery starts,
callbacks emitted inside native Stop, and fresh-session delivery. The fence is
nonblocking: callbacks already admitted before revocation may finish. It is not
a native callback join, and does not replace physical teardown/quiescence checks.

The macOS shared capture clock now uses `mach_continuous_time`, with one
process-relative epoch and checked-width tick conversion. Apple documents that
[this clock includes sleep](https://developer.apple.com/documentation/kernel/1646199-mach_continuous_time).
Both native taps initialize the timebase before native startup, keeping first-use
initialization off the audio callback. Other platforms retain their existing
monotonic clock. The legacy `epoch()`/`ns_since(Instant)` stopwatch helpers remain
available but do not describe the macOS capture epoch; capture elapsed time must
subtract `host_ns()` readings. No current recorder uses those legacy helpers.

Clock tests cover fractional timebases, intermediate-overflow avoidance,
saturation, rollback clamping, stable epoch and the public clock lying between
two direct native continuous-clock readings. The initial conversion regression
returned 9 seconds instead of the expected 300 seconds for a synthetic timebase.
This is conversion/source evidence, not a physical suspend/resume test. The
continuous clock alone cannot identify the precise suspend boundary or prevent
resumed samples arriving before wake handling; notifications and wake-gap ordering
remain required.

Device listener registration now owns its cleanup guard before adding the first
property. A later add failure rolls back every successful registration before
releasing the block/queue. A native registration-only test first proved the
second-add failure leaked a HAL-retained callback; it now checks failure at each
of the three positions and proves the captured signal is released. This opens no
audio tap and does not simulate a physical device switch.

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

This is partial implementation of #121. Quiescence of native callbacks already in flight,
suspend/wake notification boundaries and complete wake gaps, recovery of a microphone
that failed its initial open, and bounded native recovery operations remain
required. Closing the mailbox cancels queued work; it cannot interrupt a native
operation already running. Native input format
validation does not itself implement live format notifications or conversion.
Signed-app device/sleep tests and full-pipeline persisted-audio checks remain
acceptance gates; synthetic supervisor tests do not prove them.

Native Chrome fixture check: the real dashboard showed healthy system audio
beside an exhausted microphone. Keyboard activation of Retry microphone changed
it to checking, then healthy after the fixture timer; system audio stayed
healthy. The owned preview tab, server and temporary library were removed.
This validates the browser flow, not physical device recovery or microphone
permissions. The full local suite includes 46 watchdog/recovery tests and 77 UI
tests; the final embedded-web unit suite also passes.

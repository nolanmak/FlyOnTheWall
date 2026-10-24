# Native capture clock and watcher lifetime

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

This is partial implementation of #121. Physical suspend/resume, wake-boundary
ordering, native callback quiescence and bounded device operations remain open.
The clock change does not make every application timeout sleep-aware.

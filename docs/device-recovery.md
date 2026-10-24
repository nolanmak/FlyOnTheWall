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

This is partial implementation of #121. A successful start still emits
`Recovered` before sustained health is established. Accurate UI recovery state
and manual-retry wiring, generation fencing for native callbacks, sleep-aware
gaps, format-stable archive conversion, and bounded native recovery operations
remain required. Native input format
validation does not itself implement live format notifications or conversion.
Signed-app device/sleep tests and full-pipeline persisted-audio checks remain
acceptance gates; synthetic supervisor tests do not prove them.

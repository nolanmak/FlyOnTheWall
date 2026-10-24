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

This is partial implementation of #121. Failed-open retries still use the older
backoff policy, device changes can initiate separate rebuilds, and a successful
start still emits `Recovered` before sustained health is established. Unifying
these into three-attempt episodes at least fifteen seconds apart, publishing
accurate UI recovery state/manual retry, generation fencing, sleep-aware gaps,
and format-stable archive conversion remain required. Native input format
validation does not itself implement live format notifications or conversion.
Signed-app device/sleep tests and full-pipeline persisted-audio checks remain
acceptance gates; synthetic supervisor tests do not prove them.

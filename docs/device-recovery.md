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

This is partial implementation of #121. Generation fencing for native callbacks,
sleep-aware gaps, format-stable archive conversion, recovery of a microphone
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

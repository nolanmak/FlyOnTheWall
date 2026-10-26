# Native recorder control foundation

The daemon exposes in-process controls for connecting the native shell to the
same recorder used by the dashboard. These APIs are implemented; product launch,
AppKit rendering, the native consent prompt and the asynchronous command bridge
remain unfinished. The dashboard remains the shipped recording control surface.
Acceptance in issues #58 and #57 remains open.

## Control contract

- `observe()` captures authoritative status together with an opaque `StopTarget`
  under one recorder lock. The capability exists during preparation, before a
  capture session ID exists. Its fields cannot be fabricated by callers.
- `stop_observed()` checks the exact attempt identity while holding the lock that
  issues Stop. A delayed command cannot stop a replacement attempt or another
  recorder instance. The capability is in-process and cannot be serialized.
- `start_native()` requires explicit acknowledgement and accepts the actual start
  origin and a fresh per-request cancellation signal. It shares dashboard startup
  and auditing. A cancelled or unacknowledged request cannot open capture.
- Startup reserves its slot before audit/configuration I/O and releases the slot
  lock before that I/O. Observers and Stop remain responsive; another Start is
  refused while the attempt still owns preparation. Cancellation does not pretend
  to interrupt a blocked configuration function. The reservation remains until
  that function returns. Failure or unwind before session dispatch releases only
  its own reservation; dispatched sessions retain existing cleanup ownership.
- These synchronous methods belong on background workers. They are not an AppKit
  main-thread command bridge. Startup can still wait for configuration/readiness.

`native_shell::RecorderView` projects real phases, capture elapsed time, source
health, active provider/model, pending speech and per-source errors. It never
starts a speculative clock, guesses the active provider from settings, or calls
an idle recorder "Saved": persistence may still be running after the slot frees.

## Regression evidence

Behavioral REDs reproduced stale/cross-instance Stop, missing acknowledgement,
pre-cancelled capture, incorrect start origin, missing view state/source errors,
and status blocked behind configuration. Their corresponding behavior tests now
pass. Configuration-panic and audit-failure cases additionally check reservation
cleanup without opening a device.

The recording integration tests use synthetic PCM, injected configuration and
finishers, and temporary nested data roots. They do not use capture devices,
provider credentials or real meeting content. Native view tests exercise the
projection only. These checks do not establish AppKit, TCC, hotkey, focus,
physical-device or signed-app acceptance.

## Remaining product integration

Connect a bounded asynchronous command/status bridge and the AppKit surfaces,
require the native consent prompt before Start, acknowledge visible preparation
before opening capture, and enforce one shell/daemon owner. Handle stale
completions, bounded Stop/Quit, and persistence announcements without inventing
success. Then run the isolated/native and physical acceptance described in #58.

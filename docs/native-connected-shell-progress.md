# Connected native shell implementation

This branch contains the recorder bridge, native interaction model and AppKit
renderer seam. Product launch/daemon bootstrap, lifetime ownership and coordinated
Quit/finalization are still unfinished. The normal app launch still uses the
dashboard. Acceptance in #58, #57 and #124 remains open.

## Implemented contracts

- A four-command bounded queue, one Start worker and one retained status snapshot
  keep the native thread independent of recorder/configuration waits. The status
  worker publishes a monotonically ordered snapshot and the exact Stop target.
- Each queued Start has its own cancellation ticket. Old tickets, rendered
  frames and Stop targets cannot authorize a replacement attempt.
- The renderer gate is installed before exposing Start. Audit precedes capture;
  the gate then requires the actual attempt's Preparing indicator to render
  before transcription configuration or tap setup. Timeout, cancellation or a
  closed renderer refuses capture and releases its reservation.
- `SurfaceHost` connects AppKit events and views without the legacy synchronous
  assumption that a successful command dispatch means capture is already live.
  Native rendering acknowledges only after all surfaces have been applied.
- `NativeHost` requires a fresh explicit consent prompt for Menu/Hotkey Start,
  routes the actual origin through the recorder and follows dashboard-started
  recordings. It uses recorder capture time and preserves Stop until backend
  state acknowledges it. Idle does not mean Saved. A deliberately cancelled
  preparation returns to neutral idle; old action failures do not label a new
  observed capture as failed.
- The indicator has a source/status detail row and accessible full detail text.
  Unavailable measured audio level renders no meter. Preparing has a distinct
  visual register. Native appearance, clipping and focus behavior still require
  owned-view and signed-app acceptance; compilation is not visual proof.

## Tests and limitations

Behavioral RED/green logs reproduce capture before a rendered frame, missing
renderer admission, missing manual consent, stale UI errors and cancellation
shown as failure. Eleven device-free bridge/host tests cover timeout, foreign and
stale capabilities, blocked configuration, source closure, Menu/Hotkey consent,
dashboard recording visibility and clock absence. Existing recording and view
regressions remain green. Shell tests check render-before-ack ordering, failed
render without acknowledgement, legacy routing and distinct preparation icons.

Tests use synthetic PCM, injected configuration/finishers and throwaway nested
roots. No user recordings, provider credentials, capture devices, installed app,
global hotkeys or real keyboard input are used. `run_surface` is compiled, not
launched. A production launch must additionally bind one daemon/shell owner,
connect background menu actions and close HTTP admission before coordinated
Stop/drain/persistence shutdown. The current bridge Drop requests Stop; it does
not prove finalization or authorize immediate process exit.

# Native meeting offers

The connected native host consumes the existing Detector through a bounded,
read-only activity sampler. Core Audio metadata queries run on a background
worker with one outstanding query and a one-sample inbox. Neither the probe nor
the detector can open audio taps. Ordinary launch cannot start recording.

The existing conjunction, 20-second dwell, self-process exclusion and consent
rules remain in use. Native integration uses NoCalendar: EventKit and the
calendar fallback for Bluetooth remain unavailable. Manual Start remains available.

Samples use continuous host timestamps, including sleep. Samples older than three
seconds, failed queries and closed samplers withdraw detected prompts. Gaps reset
dwell. Checkbox, Start and dismissal actions revalidate the existing offer at the
click boundary; a closed publisher's final queued sample cannot retain a prompt.
A slow query cannot block AppKit or create additional outstanding queries.

Detection only offers consent. Explicit acknowledgement and Start use the shared
RecorderBridge and visible-indicator gate. The start audit carries detection-prompt,
the detected app and the shown notice. Automatic offers cannot replace manual
consent, active recording, pending Start or Quit. Withdrawal clears acknowledgement.

Not now snoozes for ten minutes. Never for this app saves a suppression in the
encrypted library and restores it before detection starts next launch. It affects
only automatic offers; manual Start and audio exclusion settings remain independent.
Both explicit dismissals append the app and answer to the local audit. Manual
Cancel and stale/withdrawn offers create no detection decline. Invalid saved
preferences disable automatic offers and preserve the saved value; manual Start
remains available.

A dedicated bounded worker performs preference/audit IO outside AppKit. Quit waits
for accepted writes. Failed writes are visible and require acknowledgement before
Quit; preference and audio-save failures have separate acknowledgements.

Device-free checks: `cargo test -p fotwd --lib detection_preferences` and
`cargo test -p fotwd --test native_bridge`. The owned `native_consent_fixture`
exercises actual AppKit Start/checkbox/dismissal controls with injected dependencies;
capture and configuration stay forbidden. It creates no tray or global shortcut.
Earlier Start-control evidence is preserved in native-detection-owned-controls;
it is tied to its recorded source, not qualification of later binaries.

Acceptance in #58, #57 and #124 remains open. Signed installed-app LaunchServices,
TCC, real-call detection accuracy, focus/fullscreen/display behavior and physical
audio/device/recovery qualification remain unverified. Owned controls do not prove
these acceptance gates.

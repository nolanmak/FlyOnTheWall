# Native meeting offers

The connected native host now consumes the existing Detector through a bounded
read-only activity sampler. Core Audio metadata queries run on a background
blocking worker, with one outstanding query and a one-sample inbox. Neither the
probe nor the detector can open audio taps. Ordinary native launch still cannot
start recording.

The existing conjunction, 20-second dwell, self-process exclusion and consent
rules remain in use. Native integration currently uses NoCalendar: EventKit and
the calendar fallback for Bluetooth remain unavailable. Manual Start remains
available when detection cannot identify a meeting.

Samples are timestamped before the query using the continuous host clock, which
includes macOS sleep. Samples older than three seconds, failed queries and closed
samplers withdraw detection prompts. An observation gap resets dwell even if a
fresh sample is waiting after a delayed UI tick. A blocked query cannot block
AppKit or coordinated sampler shutdown; at most that one background query remains
until it returns or process teardown completes.

Detection only offers a fresh consent prompt. Explicit checkbox acknowledgement
and Start enter the existing bounded RecorderBridge and visible-indicator gate.
The audit records detection-prompt, the detected app and the shown jurisdiction
notice before capture. Not now uses the detector's existing ten-minute snooze.
Probe failure cannot withdraw manual consent; active recording, pending Start and
Quit cannot be replaced by an automatic prompt. An old acknowledgement cannot
survive withdrawal and rearming. Checkbox and Start actions revalidate an existing
detected offer immediately; they cannot act on a dead sampler between UI ticks.

Two detector-inbox regressions and the native offer regression were RED before
wiring, then GREEN. A separate audit regression was RED until the detected
metadata crossed the shared admission path. A click-boundary regression was RED
when a sampler closed after the last tick could still admit a pending Start;
revalidating the existing offer before consent actions makes it GREEN. A final
queued-sample regression also proved that a closed publisher must withdraw rather
than retain the offer; the bounded mailbox read now checks that closure. All 18 native bridge regressions,
four inbox/sampler checks and strict all-target daemon Clippy passed. The blocked
probe test releases and joins its own synthetic query without touching devices.

The owned native_consent_fixture now exercises menu, hotkey and injected detector
prompts through actual AppKit controls. Unchecked Start stays disabled; the actual
checkbox enables and acknowledges Start. No recording indicator is admitted in
this fixture, so configuration and capture calls stay zero. Quit drains, and the
audit contains the actual origin and detected app. No tray/global shortcut is
registered, and the installed product is not launched. Final-source results and
source hashes are in [the owned-control manifest](native-detection-owned-controls/manifest.json).

Acceptance in #58, #57 and #124 remains open. Signed installed-app LaunchServices,
TCC, real-call detection accuracy, focus/fullscreen/display behavior and physical
audio/device/recovery qualification remain unverified. Persistent Never for this
app and decline auditing are not wired into this host, so no Never button is
exposed. These limits are not established by the injected or owned-widget tests.

# Recovering a microphone that is unavailable at startup

A meeting keeps recording system audio when its optional microphone cannot open
or start. Both the daemon and CLI retain the microphone supervisor, publish its
recovery state, and retry within the existing bounded episode policy. Initial
failure counts as attempt one. Recovery requires sustained delivery; successful
native startup alone does not establish healthy audio. Stop cancels retries.

An unavailable microphone reserves a 16 kHz mono archive track. This is an archive
policy, not an assumption about the device. On recovery the authoritative native
format feeds the fixed archive converter. Failed-start PCM and late old-producer
writes stay isolated from retained audio. A failed open that never requested a
sink also advances ring ownership correctly.

The missing startup interval is explicit in the manifest and padded as silence,
including while recording and at Stop when the microphone never recovers.
Adjacent markers for the same source/outage coalesce. The recording epoch clips
startup time that preceded the session. The healthy system track continues
unchanged. Capture conversion retains the malformed-input safeguards from #143.

Validation uses synthetic PCM, fake platforms/providers and a manual capture
clock. A fresh state-machine RED observed Stopped instead of Recovering. Earlier
persisted-audio REDs reproduced an absent startup-gap marker, a never-available
microphone whose outage was only saved at Stop, and failed-start buffers counted
as captured audio. Regression tests cover initial open failure, failed native
start with emitted PCM, recovery into a different format, retry exhaustion,
never-recovered Stop, live durable padding, old-producer isolation, intact system
audio, archive samples and timeline boundaries. They do not use a real microphone,
private recordings, credentials or a paid provider.

Native open/start/stop remain synchronous at this layer. Native operation
deadlines, sleep/format event publication, callback quiescence, exact-position
ring-overrun markers and signed-app physical USB/Bluetooth/sleep validation
remain open under #121. Provider switching during recovery still needs its
full-pipeline acceptance test. This increment does not close that issue.

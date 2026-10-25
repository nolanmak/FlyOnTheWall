# Durable speech handoff runtime

Select `fotwd stt fallback --model <name|absolute-path> [--language <code>]`
to persist an explicit Deepgram preference with an on-device backup. Both CLI
recording and dashboard Start read this choice. Deepgram remains the default;
`fotwd stt local` keeps audio on this Mac. Fallback initially sends audio to
Deepgram and stays local after a provider failure for the rest of the meeting.

Start verifies the backup file, validates its actual model/language metadata,
and prepares independent native states before capture or cloud connection.
Known model filenames must match published checksums; custom files are pinned
to their SHA-256 for recovery identity, without asserting publisher authenticity.
An unavailable backup or missing Deepgram key refuses Start. The dashboard
shows Preparing without a recording clock or red badge and offers Cancel.
Cancellation retains ownership until native preparation finishes; a late result
cannot start capture. Capture admission and cancellation share a synchronized
boundary. Once native capture setup wins that boundary, the UI says Starting
capture and offers Stop, warning that buffered audio may be saved. A late native
start still publishes capture identity/clock even if Stop woke the waiting Start
request earlier. Pending Start requests poll setup every second so Cancel does
not wait for the normal five-second status interval. No model is downloaded
automatically. Startup deadlines retain native ownership for every speech mode;
new Start requests are refused until that setup actually exits. A deadline can
bound the app's wait but cannot force a blocked OS call to return.

The dashboard Transcription panel saves the same policy, model and language.
It reports file availability without treating it as native readiness, validates
local model metadata on explicit save, and preserves the previous policy on
failure. Model work runs outside the library mutex. Summary and export settings
remain independent of speech recognition.

The recording status API and dashboard expose the actual provider/model per
source, verified revision when recorded, original coverage boundary, switch
reason, pending audio and backend capabilities. These describe the active
streams independently of the saved preference for the next recording. A durable
Complete state proves accounted input coverage, not recognition accuracy. Legacy
streams expose their actual connection lifecycle with unknown coverage rather
than claiming zero backlog or complete inference. Local-only streams also
measure worker liveness; cloud-only streams have no native-worker measurement.

Read-only durable status handles retain metadata and atomics, not model or file
leases. After a durable inference timeout, its measured native worker remains
visible and prevents another recording until cleanup finishes. Audio finishing
and library persistence proceed independently. This retained-worker admission
guard applies to both durable fallback and local-only streams, including native
recognizer teardown after a close acknowledgement. The app cannot forcibly stop
a native OS operation.

Saved meeting details expose the primary transcript's actual engine epochs,
recorded revisions and capabilities, switch reasons and confirmed receipt
boundaries. The reader does not load inference payloads or turn metadata into
transcript words. Epochs without receipts have no confirmed processed interval;
older transcripts explicitly have unknown history. A different primary transcript
does not inherit stale source history. Expand **Transcription sources** in the
saved meeting view; all boundaries use the original recording clock, independently
of the transcript's first-speech display origin.

A synthetic saved-history fixture uses real durable projection and the real API:

```sh
cargo run -p fotwd --example ui_preview -- --speech-history
```

Synthetic [desktop](speech-ui/saved-desktop.png) and
[390px](speech-ui/saved-mobile.png) views show the real saved-history response.

Startup native recovery and retry controls remain part of
[#12](https://github.com/nolanmak/FlyOnTheWall/issues/12).

## Capture and admission

`FailoverLegs::prepare` allocates an independent native state for each configured
source before capture or any cloud connection. The session subsequently binds
those states to its actual WAL identity and source anchors. Original native-format
audio remains in the capture WAL independently of transcription failures.

Each source appends exact mono 16 kHz PCM16 to its canonical replay file. Disk
catch-up reads are bounded to one second; cloud queued plus in-flight input is
bounded independently. The suppressive microphone echo gate is bypassed for this
runtime because it substitutes silence before replay storage. Genuine acoustic
echo cancellation remains separate work in #72.

## Coverage, policy and Stop

A journal records the initial engine and the prepared fallback model, revision,
language, capabilities and endpoint configuration before cloud opens. It syncs
audio before a bounded transcript receipt and syncs that receipt before publishing
its finals. Coverage is contiguous and source-specific on the original absolute
sample clock. Provider epochs fence stale results; identical words on different
sources are not duplicates. If a reconnect would replay only a clipped provider
ring, the opted-in transport stops before sending that tail. A final for clipped
audio cannot acknowledge the evicted prefix; local disk replay recovers it.

Default policy demotes immediately on authentication/quota failures, or after
three failed reconnect cycles or a 15-second retry episode. Rate/concurrency
pressure honors Retry-After within the remaining retry budget and never extends
that episode. Speech-stall demotion counts 45 seconds classified as speech by the
existing adaptive energy endpointer. It is not a neural VAD or a quality guarantee
for every acoustic environment. Demotion remains local for the meeting.

Stop syncs and seals the input before bounded drain. An input seal fixes the
source identity and final sample, and survives restart; it is distinct from proof
that transcription completed. Retirement also requires a separate acknowledgement
written after the database transaction commits, bound to the exact append-only
journal prefix. Both canonical and journal owner leases are held across retirement.
A newer journal record invalidates the acknowledgement. Promotion can publish
playable media while retaining untranscribed or unprojected raw/canonical sources.

A blocked native inference cannot be forcibly cancelled. Deadline expiry fences
late output, reports incomplete transcription, and leaves native ownership visible.
Exclusive source/journal leases prevent another attempt from reusing those files
while the previous worker is still alive.

## Explicit local restart API

`FailoverStream::resume_local` accepts a `LocalResumeConfig` with no cloud
credential or endpoint. Its worker configuration also has no cloud transport
configuration. It reopens canonical audio and coverage, checks the installed model
identity/revision/language against the saved plan, seals the existing end, and
records a `ResumePending` epoch at the last committed boundary. Only the pending
interval is inferred. A completed retry adds no artificial switch or transcript.

Wrong or missing models, a busy owner, corrupt metadata and changed sealed audio
are refusals; audio remains available for recovery. Older source journals without
a saved fallback plan remain readable for projection but cannot automatically
choose a recovery model. Asset resolution/checksum verification belongs to the
caller; a current app setting is not evidence of the original recording's choice.
The API does not implement automatic native inference at daemon startup. Existing
startup promotion does refresh already committed journal receipts into the database
using the original saved capture clock before acknowledging and retiring sources.
This repairs a crash after inference or SQL commit without reopening cloud or
replacing edits. Recovering an actively recording, unfinalized raw WAL still needs
the app's capture-clock/recovery orchestration; this source API does not finalize it.

## Library projection and exports

Schema 7 adds immutable `speech_epochs` and `speech_receipts`, with meeting and
transcript cascades. Capture/notes handoff, provenance, receipt insertion and
segment insertion share one immediate transaction. Original STT identities are
retained. An exact retry skips the projected rows, preserving human corrections,
notes, titles and primary-transcript selection. A new interval appends without
replacing the old text; conflicts or gaps roll back the whole transaction.

Receipt identity includes original word records and timestamp origin, independent
of compressed word-blob encoding. Anonymous speaker labels are namespaced by
source and provider epoch; the known microphone user remains `me`. Library JSON
archives retain all new columns and tables; older meeting documents default to
empty speech-history collections. Plaintext export remains an explicit existing
tradeoff. Meeting deletion removes original receipt text as well as projected text.

The library uses a composite `mixed` / `source-journal` transcript entry; detailed
source/provider presentation in the dashboard and summary prompts is still pending.
Existing v6 libraries migrate forward with the normal encrypted backup; older
binaries refuse a v7 library.

## Validation scope

Ordinary tests use owned synthetic audio and loopback provider servers. They cover
long replay, speech versus silence, retry policy, original source clocks/formats,
late native results, input sealing, restart, model mismatch, stable receipt
projection, archive round trips and deletion. The opt-in installed-model smoke
accepts only checksum-verified public small.en weights and the public JFK WAV; it
exercises both reconnect handoff and local restart without capture devices or paid
provider calls. These checks do not prove signed-app device recovery, human speech
quality, cold-start performance or contention between both apps. The roadmap in
[#124](https://github.com/nolanmak/FlyOnTheWall/issues/124) retains those gates.

## Inspecting the live status UI

`cargo run -p fotwd --example ui_preview -- --speech-status` serves a temporary
synthetic library with two different source engines. Stop simulates an incomplete
local worker, then releases the fixture after eight seconds. No capture, model,
provider or user library is opened. The one-use launch link expires after 30 seconds.
The preview is layout/interaction evidence; backend tests use real session pumps,
loopback transports and a controlled blocking recognizer separately.

![Synthetic live source status](speech-ui/live-desktop.png)

![Synthetic retained transcription work at narrow width](speech-ui/retained-work-mobile.png)


## Readable export history

Markdown (including OKF/GitHub's Markdown renderer) and plain-text meeting exports
carry a separate **Transcription sources** section. It uses the same selected
primary transcript as the transcript lines, includes source/provider/model,
recorded revision/capabilities, switch reasons and confirmed audio boundaries,
and preserves history even when receipts contain only silence. These metadata
boundaries stay on the original recording clock while transcript lines retain
their first-speech presentation origin. They do not certify completeness or
recognition accuracy. An engine without receipts has no confirmed processed
interval, and older transcripts explicitly have unknown history.

The formatter reads only epoch/receipt metadata, never inference payloads, and
keeps untrusted metadata on one escaped line in Markdown. JSON/archive export
continues to preserve the original rows and immutable inference payloads.
The remaining recovery/physical/quality gates remain follow-up work.


## Summary engine context

The daemon supplies actual primary-transcript engine epochs to summary prose,
structured extraction and every map/reduce instruction. Provider/model/revision,
recording-clock boundaries, switch reason and recorded timing/speaker capability
availability travel as recording facts. They do not become transcript words,
citable evidence or part of the shared system prompt/cache prefix. Empty history
explicitly means it was not recorded. The model is instructed to preserve unknown
owners and avoid carrying system speaker identities across engine epochs; these
instructions are not a measured guarantee of model attribution quality.

The metadata block is bounded to 6144 bytes, with at most 16 epochs and 96 Unicode
characters per free-text field. JSON is framed separately, delimiter characters
are escaped, and omitted epochs or clipped fields are explicitly reported.
Admission reserves the actual rendered metadata byte count against both prose
and extraction windows. Reduction also checks its combined mapped input before
calling a model. Existing citation/evidence validation still uses only original
transcript segments. Summary engine selection and egress policy remain separate
from local ASR, and a summary failure does not erase the raw transcript.

Tests use a real temporary library and fake transports, including bounded hostile
metadata, map/reduce/extraction propagation, unknown history and context rejection.
No paid-provider attribution evaluation is claimed. Native restart/retry,
provisioning, AEC and the original physical/quality acceptance remain unfinished.

## Microphone format invalidation

The native microphone capture owner installs device-rate, input stream-list and
input configuration listeners before reading the ASBD, and virtual-format
listeners on every enumerated input stream. These listeners belong to that
capture generation; their callbacks only latch an atomic invalidation flag.
An invalidated generation emits `StreamFormatChanged` before accepting more IO
under the old sample rate, even if the channel layout did not change. It never
retimes that buffer under stale metadata. Startup rejects already observed
invalidation, and Stop reports invalidation after listener removal/queue drain
even when no subsequent IO arrived. `FormatChangedAfterStop` carries positive
Released evidence only after complete teardown, so the invalid utterance is
rejected without permanently fencing a fresh deliberate attempt. A known
invalidated format is no longer reported as authoritative. A fresh attempt
reads a new format.

These listeners belong to the existing native capture resource bundle. Partial
setup unwinds through its owner. A removal failure retains the bundle and the
process microphone fence with typed cleanup evidence; it cannot be reported as
fully released. Successful teardown drains the listener queue before freeing
callback state. The system-audio leg does not receive a synthetic global route
change from this microphone-only guard.

The shared `conformance::formats()` contract on macOS drives the actual native
block, latch, IO guard and finalization check using synthetic PCM. It opens no
capture device. Additional tests cover registration-prefix ownership, failed
removal, queued notification drainage and generation isolation. The opt-in
`installed_input_listener_registration_without_capture` probe only registers and
removes metadata listeners; it never creates or starts an IOProc. On this Mac it
reported no installed default input, so real input registration and physical
USB/Bluetooth/rate-change capture validation remain unperformed.

Apple documents the [device nominal rate](https://developer.apple.com/documentation/coreaudio/kaudiodevicepropertynominalsamplerate)
and [stream virtual format](https://developer.apple.com/documentation/coreaudio/kaudiostreampropertyvirtualformat)
properties. A listener notification is treated conservatively as invalidation;
this feature does not prove real-device timing or automatic readiness recovery.
# Read-only input readiness

The macOS audio backend exposes `default_input_snapshot()` for background
readiness checks. It reads the default device's real UID and input ASBD twice,
rejects unknown identity, changing device/rate/channels and native read errors,
and applies the same packed Float32/rate/channel validation as microphone start.
It creates no capture stream, IOProc, property listener or audio buffer.

The result is a metadata observation, not permission, live audio, a locked route
or sustained health. Native property reads may block; callers must bound the
handoff, retain one worker until it returns, reject stale results and revalidate
on deliberate capture start. Repeated matching observations are the consumer's
recovery policy. Device-free injected-read tests cover instability and errors;
physical USB/Bluetooth and capture validation remain separate gates.

## Saved meeting retry in the app

A saved meeting with retained canonical audio and a recorded local recovery plan
shows **Finish transcription** and **Retry locally**. Opening the meeting only
reads status. Retry uses the original per-source model/revision/language and
absolute coverage boundary, preserves corrected committed text, and projects
receipts idempotently into the original meeting and recording clock. It does
not change today's ASR settings, read a cloud credential, start capture or invoke
a summarization engine. A completed retry does not initialize another model. Already-confirmed sources
need no local recovery plan: missing completion markers can be finalized under
exclusive file ownership, without loading a model or creating a provider epoch.

The daemon owns one retry job at a time. The HTTP request returns immediately;
metadata/model preparation has a 30-second deadline. During inference, only
a durable coverage advance renews the 120-second progress-stall budget; healthy
long recordings can run beyond that budget while making progress. Native calls cannot be forcibly interrupted. Timeout
or Cancel revokes constructor/inference output while the job retains admission
until the actual worker, model and cleanup return. Another retry cannot multiply
a stuck owner. Model loading follows exclusive source/journal admission, and no
library mutex spans model construction or inference. Missing/changed assets or
invalid capture identity/clock retain audio and report failure. The installed
candidate and managed copy are selected by saved identity and SHA-256, with
checksum checks around native context creation.

Authenticated routes under `/api/meetings/{id}/speech-recovery` expose status,
`retry` and `cancel`. Action bodies are empty objects; clients cannot choose a
cloud destination or pass a recording clock. The dashboard fences detached or
superseded replies, keeps recovery controls keyboard accessible and polls only
pending work. Successful recovery offers an explicit transcript reload so a
status update does not replace a note editor underneath the user.

Device-free tests cover repeated projection, edited text, original clocks/audio,
invalid clocks, deletion, retained constructor/inference ownership, cancellation,
authentication and stale UI replies. The ignored `speech_recovery` integration
test accepts only checksum-pinned public small.en/JFK assets and runs through the
production app controller, including refusal of a different revision. It is a
functional smoke, not a paired ASR quality or resource benchmark.

This entry point currently requires a finalized capture, its saved meeting alias
and frozen clock, and the original local plan. Automatic startup scheduling,
unfinished raw-WAL clock recovery, model provisioning and physical signed-app
Wi-Fi/device acceptance remain separate unfinished work under #12/#33/#124.

The `ui_preview --saved-speech-recovery` fixture supplies synthetic metadata only
and has no model/provider/capture capability. Private WKWebView acceptance
exercises actual authenticated GET/Retry/Cancel, desktop and 390px layouts,
keyboard focus, completion and retained cleanup. These are native renders, not
physical-device acceptance:

![Local retry with pending saved audio](speech-recovery/pending-mobile.png)

![Recovered text waits for explicit reload](speech-recovery/completed-mobile.png)

### Capture ownership and clock evidence

New WALs hold an exclusive `capture-owner.lock` for the writer's lifetime. A
recovery reader must acquire that existing lease; age, a timeout, or a missing
lease file never proves that a writer has stopped. A hard process kill releases
the lease without requiring a Rust destructor.

`capture-clock.json` records the original manifest identity/start/formats, an
observed elapsed time and fsynced raw PCM byte counts. Both recording pumps
refresh it every two seconds and before finalization. The first observed Stop
request freezes the elapsed time while later drain can extend the durable byte
counts. This timestamp proves a requested Stop, not native teardown completion.
The archival manifest's later `ended_at_ms` remains a finalization timestamp.

Clock publication flushes the audio first, fsyncs a temporary checkpoint, then
renames it atomically and syncs the containing directory on Unix. Recovery
refuses mismatched, unsupported, torn, overflowing or truncated evidence. Extra
raw bytes remain visible as uncheckpointed tails: they prevent certification of
a complete frozen clock and are never truncated. An interrupted live checkpoint
retains its original observed prefix without inventing a Stop or a recovery-time
end. Legacy recordings without this evidence remain unavailable to this new
ownership protocol; their audio is preserved.

A checkpoint metadata failure reports one capture warning and does not stop the
raw-audio pump. Later successful checkpoints can restore evidence; missing or
stale evidence cannot certify the uncheckpointed audio after a crash.

These primitives and their production checkpoints prepare unfinished-WAL
recovery. They do not yet enable automatic startup inference or reconstruct
canonical speech audio from raw PCM; the saved-meeting retry requirements above
still apply. Device-free contracts include hard process kill/reacquisition,
live-owner exclusion, Stop/drain clock preservation, tails and corrupt evidence.

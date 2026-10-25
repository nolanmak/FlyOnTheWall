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

Persisted provenance display, summary-prompt provenance, startup native recovery
and retry controls remain part of
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
Summary-prompt provenance and the remaining recovery/physical/quality gates
remain follow-up work.

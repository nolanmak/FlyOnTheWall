# Durable speech handoff runtime

This backend supports an explicit Deepgram-to-local session through
`session::Transcription::Failover`. App settings still select Deepgram or local
transcription; the app does not automatically select the new runtime. Persisted
policy selection, model resolution, startup recovery and status/retry UI remain
part of [#12](https://github.com/nolanmak/FlyOnTheWall/issues/12). This document
describes the backend contract, not a finished app feature.

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
sources are not duplicates.

Default policy demotes immediately on authentication/quota failures, or after
three failed reconnect cycles or a 15-second retry episode. Rate/concurrency
pressure honors Retry-After within the remaining retry budget and never extends
that episode. Speech-stall demotion counts 45 seconds classified as speech by the
existing adaptive energy endpointer. It is not a neural VAD or a quality guarantee
for every acoustic environment. Demotion remains local for the meeting.

Stop syncs and seals the input before bounded drain. An input seal fixes the
source identity and final sample, and survives restart; it is distinct from proof
that transcription completed. A separate completion proof permits retirement only
when the entire aligned canonical stream was accounted for. Promotion can publish
playable media while retaining incomplete raw/canonical sources.

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
The API does not implement an automatic daemon-startup recovery workflow.

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

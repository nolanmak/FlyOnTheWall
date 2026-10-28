# Post-meeting transcript cleanup (#246)

After Stop, in local mode, FlyOnTheWall transcribes the meeting a second time
from its saved audio, with more context than the live path had and with
speaker labels for the other participants. The result is a **new transcript
version** shown in place of the live one. The live version is kept.

Code: `crates/fotwd/src/post_pass/` (the job), `crates/fotw-store/src/post_pass.rs`
and `schema/0009_post_pass.sql` (jobs and checkpoints),
`crates/fotw-web/src/cleanup.rs` and `ui/cleanup.js` (the meeting view and the
setting). Engines: `docs/SPEECHD_PROTOCOL.md`.

## What runs

For each of the meeting's two archived Opus tracks (`media/…/mic.opus`,
`system.opus`), decoded a packet at a time so an hour of audio is never in
memory:

1. **Windows.** 30 s windows with 2 s of overlap
   (`post_pass/windows.rs`, after Muesli's `RecordingAudioWindowReader`). Only
   one window of PCM exists at a time.
2. **Speech detection.** Silero VAD v5.1.2, the same pinned ggml detector that
   gates live local transcription (#242). A window with no speech is never
   transcribed; inside one with speech, audio farther than 300 ms from
   speech is silenced in place, so times are unchanged.
3. **Parakeet Unified** in the speech helper, with `options.timings`, which
   returns each word's start and end.
4. **Checkpoint.** The window's words are written to the library with the
   job's progress in one transaction.

Then:

5. **Boundary-only merge** (`post_pass/merge.rs`, after Muesli's
   `removingOverlap`). Only the 2 s shared by two windows is reconciled, never
   the body of a window. A word heard cut off at a window's edge is taken from
   the window that heard it whole; the rest of the shared stretch is aligned
   by text (at most 8 words, times within 600 ms) and kept once; where the two
   windows heard it differently, the cut falls at the overlap's middle.
6. **Diarization** of the system track (remote speakers) by the helper's
   `diarizer` engine (FluidAudio `DiarizerManager`), streamed in 10 s windows
   and paced by its acknowledgements. Speaker labels (`S1`, `S2`, …) belong to
   this meeting's system channel only. The mic channel is `me`.
7. **Turns** (`post_pass/turns.rs`, after Muesli's `MicTurnNormalizer`,
   `SystemTurnNormalizer` and `TranscriptReconciler`): each system word takes
   the speaker whose segment overlaps it most; a channel's words join into a
   turn across a gap of at most 0.35 s, or 1.5 s when either side is a short
   fragment, and a turn breaks where the speaker changes or at a sentence end
   after 20 s. Every word is kept. The live transcript's cross-channel echo
   pass then runs on the result.
8. **Store** the turns as a new transcript (`provider` `fotw-speechd`, model
   `parakeet-unified+diarizer`), make it primary, mark the job done with its
   provenance and delete the checkpoints: one transaction.

If the diarizer is not installed or fails, the cleaned version is stored
without remote speaker labels and the job's detail says so. If Parakeet or the
speech detector is missing, the job fails with how to install it, and the live
version stays.

## When it runs

`Transcription settings → Clean up the transcript after a meeting`
(`/api/settings/cleanup`, settings row `post_pass`):

| setting | runs for |
|---|---|
| Automatic (default) | meetings no cloud provider transcribed: no transcript and no speech epoch of the meeting names Deepgram, so a fallback meeting that used Deepgram for any stretch is not cleaned |
| Always | every meeting, Deepgram ones included |
| Off | nothing |

With Deepgram the live text is already final quality, and a cloud re-pass
would double the cost; Always re-transcribes on this Mac, it never calls a
cloud provider. A meeting with no archived audio is never queued.

## Versions

The cleaned transcript is a new row in `transcripts`; the live one is not
touched. `post_pass_jobs` names both (`live_transcript_id`,
`cleaned_transcript_id`). **Show live transcript** in the meeting view makes
the live version primary again, and **Show cleaned transcript** switches back
(`POST /api/meetings/{id}/cleanup/show`). If someone switched versions while a
cleanup ran, its result does not take over.

Exports follow the shown version and add the other: Markdown and plain text
list both under **Transcript versions** with their engines, revisions and
window sizes, and append the version not shown under its own heading. JSON and
library archives carry both transcripts and the job row.

## Ordering: summaries and the GitHub push

The recording tail queues the job after the tracks are archived and waits for
it before enrichment (title, summary, action items), for at most
15 minutes (`POST_PASS_WAIT_MS`). After that it summarizes the live version
and the cleanup still finishes. The GitHub push already waits for
enrichment's receipt, so it pushes whichever version enrichment used. The
background summary queue skips a meeting while its pending cleanup is younger
than 15 minutes.

## Restart

Jobs are rows, and each finished window is a checkpoint. A daemon that quits,
crashes or is killed resumes pending jobs at its next start from the first
window without a checkpoint: no finished window is transcribed again, and
because the merge is a pure function of all windows' words, the result is
identical to an uninterrupted run. A window in flight when the daemon died had
no result and is transcribed once on resume. Diarization is not checkpointed
(speaker identities belong to one diarizer stream), so an interrupted
diarization restarts that channel; it is the short phase. A job whose engines'
pinned revisions changed between runs fails rather than mixing models.

Engine failures are retried up to three times per daemon run, then the job
is marked failed.

## Recovery of interrupted recordings is a separate thing

"Retry locally" and startup recovery (docs/DURABLE_SPEECH.md) finish the
**live** transcript of canonical audio that was never transcribed, with each
source's saved plan and the original sample clock, and append to the meeting's
existing transcript. They still refuse Parakeet plans ("Parakeet recovery isn't
supported yet; the audio is kept"), because recovery replays an exact saved
per-source model plan through the durable journal, and that path runs only the
in-process Whisper engine.

The cleanup reads only finished, archived meetings and always writes a new
version. It does not depend on the recording's speech plan, so a meeting whose
live Parakeet transcript was cut short still gets a complete Parakeet
transcript from its saved audio once it is in the library: that is how saved
Parakeet audio is re-transcribed today. Extending recovery itself to Parakeet
plans can reuse the same helper engine and is left to #12/#124.

## Tests

Ordinary suite (offline, fakes):

- `post_pass::merge` — a word straddling every window edge of a 10-minute
  track is kept exactly once, also when the cut-off copy is misheard and when
  windows disagree on times by 40 ms; repeated words in a window's body are
  never removed.
- `post_pass::job` — both channels' words land once with system speakers
  labelled; **a job killed halfway (a window in flight) resumes, transcribes
  no finished window again, and stores exactly what an uninterrupted run
  stores**; no diarizer → unlabelled, with the reason; stop keeps checkpoints.
- `post_pass::tests` — a Deepgram meeting (or a fallback meeting with any
  Deepgram epoch) gets no job unless cleanup is set to Always; Off never runs.
- `post_pass::controller` — queue → worker → done from archived Opus; a
  missing model fails plainly; the tail stops waiting at its limit.
- `post_pass::web`, `fotw-store` `post_pass` — reversible versions, exports
  with provenance, deletion removes checkpoints, archives round-trip.
- `crates/fotw-web/tests/ui/cleanup.cjs` — progress, reload on completion,
  the live/cleaned switch, the setting.

Real models: `crates/fotwd/examples/post_pass_eval.rs` (acceptance below).

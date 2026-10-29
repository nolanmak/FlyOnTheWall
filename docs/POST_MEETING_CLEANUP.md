# Post-meeting transcript cleanup (#246)

After Stop, in local mode, FlyOnTheWall transcribes the meeting a second time
from its saved audio, with more context than the live path had and with
speaker labels for the other participants. The result is a **new transcript
version** shown in place of the live one. The live version is kept.

Code: `crates/fotwd/src/post_pass/` (the job), `crates/fotw-store/src/post_pass.rs`
and `schema/0011_post_pass.sql` (jobs and checkpoints),
`crates/fotw-web/src/cleanup.rs` and `ui/cleanup.js` (the meeting view and the
setting). Engines: `docs/SPEECHD_PROTOCOL.md`.

## What runs

For each of the meeting's two archived Opus tracks (`media/…/mic.opus`,
`system.opus`), decoded a packet at a time so an hour of audio is never in
memory:

0. **Cross-track bleed suppression** (`post_pass/bleed.rs`). Each track is
   read in step with the other one. Wherever it is, for the moment, only a
   quieter copy of the other track (the person at this Mac heard again in
   the remote side's audio, or the remote side heard again in the mic), those
   samples are set to zero in place, so times are unchanged. Everything
   below, the diarizer included, hears the result. See
   [Cross-track bleed](#cross-track-bleed).
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
   and paced by its acknowledgements. Parakeet is unloaded first and the
   diarizer loaded only then, so the two models are never in memory
   together; when the job ends, however it ends, it unloads what it loaded.
   A model a recording loaded (live Parakeet) is left loaded, the cleanup
   waits while a recording runs before unloading, and if a recording starts
   at that moment Parakeet is loaded again for it. Speaker labels (`S1`, `S2`, …) belong to
   this meeting's system channel only. The mic channel is `me`.
7. **Turns** (`post_pass/turns.rs`, after Muesli's `MicTurnNormalizer`,
   `SystemTurnNormalizer` and `TranscriptReconciler`): each system word takes
   the speaker whose segment overlaps it most; a channel's words join into a
   turn across a gap of at most 0.35 s, or 1.5 s when either side is a short
   fragment, and a turn breaks where the speaker changes or at a sentence end
   after 20 s. Every word is kept. The live transcript's cross-channel echo
   pass then runs on the result.
8. **Store** the turns as a new transcript (`provider` `fotw-speechd`, model
   `parakeet-unified+diarizer`), make it primary (if it finished within the 15-minute wait), mark the job done with its
   provenance and delete the checkpoints: one transaction.

If the diarizer is not installed, will not load or fails, the cleaned version
is stored without remote speaker labels and the job's detail says so. If Parakeet or the
speech detector is missing, the job fails with how to install it, and the live
version stays.

## When it runs

`Transcription settings → Clean up the transcript after a meeting`
(`/api/settings/cleanup`, settings row `post_pass`):

| setting | runs for |
|---|---|
| Automatic (default) | meetings no cloud provider transcribed (on AMI the cleaned version is not better than live on every meeting: TS3003c ties live at 11.8% WER; see Acceptance): no transcript and no speech epoch of the meeting names Deepgram, so a fallback meeting that used Deepgram for any stretch is not cleaned |
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
(`POST /api/meetings/{id}/cleanup/show`). `POST …/cleanup/cancel` and
`POST …/cleanup/retry` (empty JSON bodies) back **Cancel cleanup** and
**Retry cleanup**. If someone switched versions while a
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

Failures back off and can be retried (below).

## Upgrading and going back

The cleanup's tables are library schema 11. A build from before it refuses
to open an upgraded library (forward-only migrations, docs/UPGRADE_CHECKPOINT.md).
To go back, restore the encrypted `backups/pre-migration-10-*.db` the upgrade
took automatically (meetings recorded since are not in it), or import a library
archive made before upgrading.

## Failures, retry and cancel

A failure (the engine will not start, the helper crashes, a track cannot be
read) is retried after 30 s, 2 min and 10 min; after the fourth the job is
marked failed and the meeting shows **Retry cleanup**. A job waits between
windows while a recording is running. **Cancel** stops a job before its next
window and keeps the live transcript; turning the setting off does not cancel
jobs already queued.

A cleanup that finishes after the 15-minute fallback is stored but not made
primary, since the summary and the GitHub push were made from the live
version; the meeting says a cleaned version is available.

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
plans is tracked in [#274](https://github.com/nolanmak/FlyOnTheWall/issues/274).

## Tests

Ordinary suite (offline, fakes):

- `post_pass::merge` — a word straddling every window edge of a 10-minute
  track is kept exactly once, also when the cut-off copy is misheard and when
  windows disagree on times by 40 ms; repeated words in a window's body are
  never removed.
- `post_pass::job` — both channels' words land once with system speakers
  labelled; **a job killed halfway (a window in flight) resumes, transcribes
  no finished window again, and stores exactly what an uninterrupted run
  stores**; no diarizer → unlabelled, with the reason; stop keeps checkpoints;
  the diarizer hears the remote person but not the mic speaker's copy;
  Parakeet is released before the diarizer starts and everything after the
  job (also a stopped one); a recording that starts at the release gets
  Parakeet back, and one still running when the job ends keeps it.
- `post_pass::bleed` — an attenuated, delayed copy of the other track is
  silenced (over 90% of it); a different person's quiet speech under the
  mic speaker, and a different person talking over the copy (where louder
  than it by 3 dB), are kept (over 99%); nothing is removed while the other
  track is silent; the output is the same however the track is read.
- `post_pass::speechd_engine` (fake helper) — Parakeet is unloaded before the
  diarizer loads and nothing stays loaded; a model a recording loaded is left
  loaded; a released recognizer can be restored.
- `post_pass::tests` — a Deepgram meeting (or a fallback meeting with any
  Deepgram epoch) gets no job unless cleanup is set to Always; Off never runs.
- `post_pass::controller` — queue → worker → done from archived Opus; a
  missing model fails plainly; the tail stops waiting at its limit.
- `post_pass::web`, `fotw-store` `post_pass` — reversible versions, exports
  with provenance, deletion removes checkpoints, archives round-trip.
- `crates/fotw-web/tests/ui/cleanup.cjs` — progress, reload on completion,
  the live/cleaned switch, the setting.

Real models: `crates/fotwd/examples/post_pass_eval.rs` (acceptance below).

## Cross-track bleed

`post_pass/bleed.rs`. A meeting's two tracks are not independent. The remote
side's audio can carry a quiet copy of the person at this Mac (their
speakerphone or room system picking them up, a call app playing the local
voice back), and the mic can carry a quiet copy of the remote side. The
live path rarely transcribes such a copy; the cleanup, hearing 30 s at a
time, does, and the diarizer hears it as one more remote voice. On the AMI
replays, where every headset hears its neighbours, this was most of what kept
the cleanup above its WER and DER gates.

Each track is read in step with the other one (the reference), in 10 ms
frames of log energy. A frame is silenced when all of these hold:

1. the reference is speaking (≥ 30 dB re 1 LSB², averaged over ±50 ms) and
   the track is at least 14 dB below it;
2. over ±1 s, counting only frames where the reference speaks, the two
   loudness envelopes rise and fall together (Pearson ≥ 0.6) at a lag of at
   most 10 ms (an acoustic or electrical path, not a network echo);
3. the frame is no louder than the copy would be: at most 6 dB above the
   reference less the copy's attenuation, which is the 20th percentile of
   track minus reference over that second. Someone else talking over the
   copy makes the track louder than that, so they are kept.

Removal extends 200 ms either side of such a frame, but only over frames that
pass 1 and 3. The decision for a sample depends only on audio within a fixed
distance of it, and each 10 s block is read with that much context, so the
result does not depend on how the track is read.

What it does not do: it silences, it does not subtract. Where a remote person
talks over the copy and is not clearly louder than it, they go with it.
Echo that returns over a network a few hundred ms late is not searched for.

**Tuned on held-out meetings.** The thresholds were chosen on three AMI
meetings that are not acceptance meetings and come from other groups of
speakers: ES2005c (mic headset 2), IS1003c (1) and TS3006c (1), 120–720 s,
SHA-256s in `tools/speech-eval/bleed-tuning-set.json`. The sweep (correlation
0.5–0.8, level gap 10–18 dB, hang 50–200 ms, window ±0.5–1 s, lag ±10–30 ms,
system track only or both) had a broad plateau; the chosen settings scored:

| tuning meeting | live | cleaned, no suppression | cleaned, suppression | system-only DER, before → after |
|---|---|---|---|---|
| ES2005c | 19.5% | 18.9% | 17.4% | 17.2% → 17.2% |
| IS1003c | 64.5% | 28.1% | 28.5% | 21.9% → 17.5% |
| TS3006c | 21.0% | 23.2% | 18.6% | 13.6% → 12.0% |

Before choosing, the detector was also checked against unrelated real
speech: the envelopes of two different meetings' mic speakers, where both
talk, correlate at ≥ 0.6 in about 1% of frames with this window and lag,
which is why the level conditions exist. The acceptance meetings
were then run once with these settings, untouched.

## Acceptance (measured 2026-10-29, M3 MacBook Air 24 GB, AC power)

Real models: Parakeet Unified `d32e972`, Silero v5.1.2 (ggml), diarizer
`speaker-diarization-coreml@df2625a` (FluidAudio 0.17.3's pin; license
unconfirmed, see #277), helper built from FluidAudio 0.17.3. Fixtures:
`tools/speech-eval/ami_replay.py` (#267): AMI ES2004c, IS1009c and TS3003c,
120–720 s. The mic track is the #118 mic headset; the system track is the
sum of the other three headsets ÷ 2. The reference is **every speaker's
words in time order** (the corrected #118 method). Runner:
`crates/fotwd/examples/post_pass_eval.rs`; scorer: `meeting_score.py`.
"Before" is this build's parent (main at b54fab1) run the same day on the same
machine; it differs from the 2026-10-28 figures (16.1/19.8/17.3%) by run to
run variation of the helper.

**WER, all-speaker reference** (gate: cleaned ≤ live and ≤ 15%):

| meeting | live | cleaned before | cleaned after | gate |
|---|---|---|---|---|
| ES2004c | 30.6% | 17.0% | 16.9% | fails ≤ 15% |
| IS1009c | 93.3% | 21.4% | 19.0% | fails ≤ 15% |
| TS3003c | 11.8% | 18.4% | 11.8% | passes (133 errors, the same count as live) |

Suppression removed what it was meant to: system-channel insertions fell from
2/673/69 to 2/5/14, and IS1009c's mic channel from 90.9% to 18.2% WER. What
keeps ES2004c and IS1009c above 15% is mostly **deletions** (198 and 129
words): overlapping speakers summed into one system track, where Parakeet
transcribes the dominant one, and turn ordering across overlap.

**#241's fixture** (AMI ES2004c Headset-2, 120–720 s, which local Whisper
emptied): 663 reference words, WER 7.5%, deletions 3.8%
(`crates/fotwd/tests/post_pass_real.rs`; mic track only, so no suppression).

**DER** of the diarizer's spans, system speakers only, pyannote-style with
overlap scored, gate ≤ 25% at ±0.25 s per reference boundary (pyannote
`collar=0.5`):

| meeting | before | after | false alarm before → after |
|---|---|---|---|
| ES2004c | 60.9% | 17.9% | 147 s → 5 s |
| IS1009c | 132.4% | 76.2% | 235 s → 130 s |
| TS3003c | 87.2% | 23.1% | 204 s → 29 s |

ES2004c and TS3003c pass; IS1009c fails. Its remaining false alarm is the
mic speaker's crosstalk too, but in IS1009c's room it reaches the other
headsets louder: while only the mic speaker talks, the system track is a
median 14.5 dB below the mic over the whole meeting (ES2004c: 28.5 dB), so
condition 1 keeps about half of it. The threshold was not
moved for it: it was set on the tuning meetings.

The speaker threshold (0.6) was chosen earlier by sweeping 0.84 (FluidAudio's
default), 0.7, 0.6, 0.5 and 0.4 on these same three meetings, so it is tuned
on the acceptance set (DER counting every voice in the system track, without
suppression):

| threshold | ES2004c | IS1009c | TS3003c (every-voice DER) |
|---|---|---|---|
| 0.84 | 38.0% | 27.9% | 33.0% |
| 0.7 | 25.1% | 10.0% | 14.5% |
| 0.6 | 19.5% | 10.2% | 14.5% |
| 0.5 | 20.8% | 12.4% | 15.0% |
| 0.4 | 22.1% | 13.9% | 15.5% |

**Speed** (gate ≤ 6 min). 20 runs of a 60:00 two-track replay (ES2004c whole,
then IS1009c), Opus archives pre-encoded so only the cleanup is timed, AC
power, load average 3–8 (other apps open): **p50 93.5 s, p95 106.3 s**
(RTF 0.026 / 0.030; min 82.1 s, max 107.3 s). Main's build, 5 runs of the
same: p50 98.1 s, max 116.4 s.

**Memory** (gate ≤ 1.5 GiB including Core ML's wired memory).

- Resident, over the same 20 runs: daemon side p50 42 MB / p95 55 MB, helper
  p50 120 MB / p95 123 MB (peak physical footprint 170 MB). The runner no
  longer reads the WAVs when the archives exist, so the daemon-side figure is
  the cleanup's alone (the 157 MB reported before was mostly the runner's own
  PCM).
- Core ML keeps model weights wired outside both processes. Measured directly
  (`post_pass_eval --model-memory 10`: load and warm each model in a fresh
  helper, settle 3 s, read `vm_stat`, unload; medians of 10 rounds): Parakeet
  **626 MiB**, the diarizer **about 30 MiB** (inside the noise), both at
  once 653 MiB, and back to the idle level after both are unloaded.
- System-wide wired memory during each 60-minute run rose by p50 986 MiB,
  p95 1,622 MiB (min 687, max 2,270) above its level 5 s before. This
  counts anything else on the machine that wired memory meanwhile (other
  apps were open); two of the 20 runs are the outliers.

Counting resident memory plus the system-wide wired rise, the cleanup's peak
is **p50 1.11 GiB, p95 1.74 GiB**: under the gate at the median, over it at
p95, because the wired rise cannot be attributed to the cleanup alone on a
busy machine. Counting resident memory plus the models' own measured wired
memory, it is about 0.8 GiB.

What changed: the diarizer is loaded only after Parakeet is unloaded, and
the job unloads what it loaded when it ends. The first saves little at peak
(the diarizer is small); the second matters after the job: before, both
models stayed wired in the daemon's helper until it exited.

The 10-minute meetings each cleaned in 10–14 s (RTF 0.02).

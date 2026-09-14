# Preventing forgotten recordings

A recording can outlive its call by a long way. In one library, a call whose
conversation ended after about half an hour was stored as a recording of more
than 22 hours, with stray transcript segments nearly two hours in. Recording
duration and speech duration are different.

## Implemented

- UI recordings stop automatically after two hours (previously eight).
- The session checks wall time every second as well as retaining its
  monotonic timer. An expired deadline is caught within approximately one
  second of the runtime resuming after sleep. Closing the browser does not
  disable this check. A backwards clock correction cannot extend the
  monotonic limit; a large forwards correction may stop a recording early.
- The UI shows the automatic-stop countdown and emphasizes the final five
  minutes. Pressing Stop still ends a meeting immediately.
- Automatic expiry enters the finishing state and freezes the displayed
  clock before persistence and audio encoding.

The previous timer alone does not guarantee an elapsed wall-time bound:
[Rust documents that Instant may exclude system suspension](https://doc.rust-lang.org/std/time/struct.Instant.html).
Sleep is a plausible explanation for the observed overrun, not a proven
diagnosis of this particular session. No code can execute while the machine
is suspended; the check runs when its runtime resumes. A wedged runtime or
audio driver still requires independent process/OS supervision.

The two-hour limit is currently fixed for the UI (`UI_CEILING`). Longer
scheduled recordings remain available through the existing explicitly timed
CLI path. A per-session duration selector and an explicit “extend 30 minutes”
action would be the next UX improvement; extension should never happen
automatically because audio is still arriving.

## Transcript-based end detection: recommended next step

Use an end-of-call candidate plus corroborating evidence, with a visible
countdown and Keep recording / Stop now controls. Keep the hard duration
limit independent of transcription or model availability.

What that recording's transcript shows:

- An early wrap-up is not the end. The conversation resumed with planning
  after the first “I think that's everything”; stopping there would lose
  decisions made afterwards.
- The strongest boundary is a closing exchange: thanks, mutual well-wishes
  and a final goodbye. Retain that entire final segment.
- What follows the goodbye (unrelated replies, a different conversation)
  reinforces the boundary. A later discussion of related subject matter
  still does not make it part of the original call.
- Every speaker had the same label because the call used one microphone.
  A speaker-change or remote-audio-only rule would not work here.

Suggested policy to validate before enabling semantic auto-stop:

1. Detect a closing exchange within a short rolling transcript window.
2. Wait 60–90 seconds for corroboration: a reliable meeting-disconnected
   signal, sustained absence of conversation, or strong evidence that a
   different interaction has begun. Continued meeting discussion cancels the
   candidate. Ordinary pauses and isolated farewell words do not stop capture.
3. Show a 60-second stop countdown, with an explicit Keep recording action.
   For weak evidence, show a suggestion only. For strong evidence, a future
   opt-in mode can stop when the countdown expires, even with the tab closed.
4. Preserve a suggested trim boundary at the farewell. Save a derived clean
   version without deleting the source or including later private chatter in
   its summary. The cutoff must align with the end of a transcript segment.

An inactivity guard could also offer a warning after five minutes with no
detected speech and stop after ten, with a presentation-mode override. Use
audio speech activity, not merely missing STT results: a disconnected
provider can produce no words during an active meeting. Background music
and television can defeat inactivity guards, so they cannot replace a cap.

Test semantic detection on closing-then-resuming conversations, someone
leaving a group call, quoted goodbyes, long presentations, disabled/failed
STT, multiple speakers on one mic, and a second call beginning immediately.
Treat transcript text as untrusted input to any classifier; it must have no
tools or authority to change settings or export data. This change implements
the deterministic time limit, not semantic classification or inactivity stop.

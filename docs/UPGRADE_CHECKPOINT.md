# Upgrade checkpoint

Current checkpoint: FlyOnTheWall main at `5da188713583b3365827160392552d7c0c5c3e57`,
including [#212](https://github.com/nolanmak/FlyOnTheWall/pull/212). This records a
specific tested source checkpoint, not a published release or laptop acceptance.

The dashboard has local/offline-backup selection, explicit model setup, durable
speech recovery, live/saved editable notes, revision conflict handling and
recording-time anchors. The Muesli-inspired refresh includes responsive reading
surfaces and bounded transcript rendering. Existing provider choices remain.

The checkpoint passed full local repository checks, 151 UI tests, and all 15
final hosted CI jobs for #212. Owned nonpersistent WebKit checks exercised the
real temporary encrypted backend and notes recovery at wide/narrow widths. The
[notes](notes-editor.md), [dashboard](dashboard-refresh.md), and
[durable speech](DURABLE_SPEECH.md) documents give the measured scope and limits.

## Recommendation for an existing working installation

Try this checkpoint in a short noncritical session while retaining the working
installation. It has enough implemented improvements to justify a trial; the
remaining roadmap does not need to finish before useful laptop QA starts. A
working old version remains the fallback until the actual laptop workflow passes.

1. Preserve the existing app and a consistent backup of its library/configuration
   before the trial. An old app alone is not a complete rollback if persistent
   data has changed. Follow the existing library backup/recovery instructions;
   keep recovery material private.
2. Build and sign on the target laptop using its existing signing identity and
   the documented [build instructions](../README.md#build-and-check). There is no
   published signed/notarized release package. A development build verified on
   another Mac does not establish this laptop's permissions or compatibility.
3. Run one version at a time. Keep current provider/settings for the first
   baseline test, then deliberately try local/fallback and any optional features.
4. Use a disposable test meeting: verify microphone and computer audio, Stop,
   saved transcript, reopening, note save/reload, search and export. Test offline
   behavior after model setup. Check a representative call before relying on the
   upgrade for important meetings.
5. Record what was actually observed, including target Mac/macOS, source commit,
   provider/model, device, permission or failure messages, and whether the saved
   result contains both sides. Avoid including real meeting content or keys in
   public issue reports.

## Remaining release work

- The native menu bar and floating recording indicator are still not connected
  to the product launch path. The dashboard is the recording control surface.
  [#58](https://github.com/nolanmak/FlyOnTheWall/issues/58) requires authoritative
  recorder state, shared consent/start/stop gates and single-instance ownership.
- Genuine AEC still needs backend qualification, especially overlapping speech.
  Runtime wiring and signal tests do not establish acoustic acceptance.
- Signed-app device disconnect/reconnect, Bluetooth, sleep/wake, long recording,
  offline/fallback, and concurrent-app acceptance remain open. Short public-audio
  and synthetic fixtures do not prove real microphone or meeting reliability.
- Notes/UI still require the remaining native keyboard/IME, accessibility and
  physical recording-clock acceptance. Unacknowledged edits have no crash guarantee.
- Paired provider/model quality and broader resource measurements remain open in
  [#118](https://github.com/nolanmak/FlyOnTheWall/issues/118).

The full [roadmap #124](https://github.com/nolanmak/FlyOnTheWall/issues/124) remains
active. This checkpoint is not overall LGTM, release validation, or a promise that
an upgrade cannot interrupt a working installation.

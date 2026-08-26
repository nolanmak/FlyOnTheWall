// The whole client. Plain DOM, no framework, no build step.
//
// Two rules, both from docs/REQUIREMENTS.md 10.1, and both of which the Rust
// side has a test for:
//
//   ING-08  The bearer token lives in localStorage and nowhere else. Never a
//           cookie: RFC 6265 scopes cookies by *host*, so a cookie set by
//           127.0.0.1:8737 would be sent to every other service on every
//           other port of 127.0.0.1 -- every local dev server, every other
//           app's helper process. localStorage is keyed by the full origin
//           including the port, and it is not ambient: nothing attaches it to
//           a request unless this file does. localStorage rather than
//           sessionStorage so that, paired with the daemon's stable default
//           port, every tab at this origin shares one login and a bookmark
//           works -- a rebound page is a different origin and reads nothing,
//           and a daemon restart rotates the bearer, so a stale copy here is
//           worth exactly one 404.
//
//   ING-11  Transcript text is attacker-influenced. Anyone in the meeting can
//           say anything, and a calendar description can carry markup. Every
//           string from the API reaches the DOM through textContent, and the
//           markup-assigning DOM properties appear nowhere in this file --
//           `crates/fotw-web/src/assets.rs` has a test that greps for them,
//           which is why they are not spelled out in this comment either.

const TOKEN_KEY = "fotw.token";

const el = {
  search: document.getElementById("search"),
  list: document.getElementById("list"),
  detail: document.getElementById("detail"),
  status: document.getElementById("status"),
  live: document.getElementById("live"),
  elapsed: document.getElementById("elapsed"),
  consent: document.getElementById("consent"),
  consentLabel: document.getElementById("consent-label"),
  record: document.getElementById("record"),
  recording: document.getElementById("recording"),
  ghSettings: document.getElementById("gh-settings"),
  ghRepo: document.getElementById("gh-repo"),
  ghBranch: document.getElementById("gh-branch"),
  ghPrefix: document.getElementById("gh-prefix"),
  ghAuto: document.getElementById("gh-auto"),
  ghEnabled: document.getElementById("gh-enabled"),
  ghSave: document.getElementById("gh-save"),
  ghRepoList: document.getElementById("gh-repo-list"),
  sumSettings: document.getElementById("sum-settings"),
  sumKind: document.getElementById("sum-kind"),
  sumBinary: document.getElementById("sum-binary"),
  sumDisclosure: document.getElementById("sum-disclosure"),
  sumAck: document.getElementById("sum-ack"),
  sumEnabled: document.getElementById("sum-enabled"),
  sumSave: document.getElementById("sum-save"),
  sumStatus: document.getElementById("sum-status"),
};

// --------------------------------------------------------------- ING-10

// The launch URL is `http://127.0.0.1:<port>/?t=<handoff>`. `open(1)` put that
// URL in the process argv, where any process of this user can read it with
// `ps`, and the browser has already written it to history, which syncs. So:
// spend the token immediately, then remove it from the address bar with
// replaceState so a later copy-paste or bookmark does not carry it.
async function redeemHandoff() {
  const params = new URLSearchParams(window.location.search);
  const handoff = params.get("t");
  if (!handoff) return;
  history.replaceState(null, "", window.location.pathname);

  const res = await fetch("/api/handoff", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: handoff }),
  });
  if (!res.ok) return;
  const body = await res.json();
  localStorage.setItem(TOKEN_KEY, body.token);
}

function token() {
  return localStorage.getItem(TOKEN_KEY);
}

async function api(path, options) {
  const opts = options || {};
  const headers = Object.assign({}, opts.headers, {
    authorization: "Bearer " + token(),
  });
  const res = await fetch(path, Object.assign({}, opts, { headers }));
  if (!res.ok) throw new Error("request failed");
  return res.json();
}

// ---------------------------------------------------------------- render

function text(tag, value, className) {
  const node = document.createElement(tag);
  node.textContent = value === null || value === undefined ? "" : String(value);
  if (className) node.className = className;
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function when(ms) {
  if (!ms) return "";
  return new Date(ms).toLocaleString();
}

function duration(ms) {
  if (ms === null || ms === undefined) return "";
  // Rounding to minutes reported a 40-second meeting as "0 min", which reads
  // as a bug in the recorder rather than as a short meeting.
  if (ms < 60000) return Math.max(1, Math.round(ms / 1000)) + "s";
  const mins = Math.round(ms / 60000);
  return mins < 60 ? mins + " min" : Math.floor(mins / 60) + " h " + (mins % 60) + " min";
}

// mm:ss from the start of the meeting, so a segment can be found in the audio.
function offset(ms) {
  const total = Math.floor((ms || 0) / 1000);
  const mins = Math.floor(total / 60);
  const secs = total % 60;
  return mins + ":" + String(secs).padStart(2, "0");
}

// A meeting still recording has no duration yet, and showing its state is more
// useful than showing nothing. Previously written as `duration(...) || state`,
// which silently swapped the two whenever duration formatted to a falsy
// string -- so an identically-seeded library showed "ready" for one meeting
// and "0 min" for the next.
function listMeta(m) {
  if (m.state && m.state !== "ready") return m.state;
  return duration(m.duration_ms) || m.state || "";
}

// ------------------------------------------------------------- markdown
//
// A deliberately small subset: headings, bullets, and paragraphs. It builds
// DOM nodes and assigns textContent, exactly like every other renderer here,
// so ING-11 still holds -- a summary is model output written over
// attacker-influenced transcript, and handing that to a markdown library that
// emits HTML is precisely how a transcript becomes script. Rendering it as
// preformatted text was safe but showed users literal "## " on every heading.
//
// Anything unrecognised falls through as a paragraph, verbatim. That is the
// right failure: unstyled real text beats swallowed text.
function renderMarkdown(md, into) {
  let list = null;
  for (const raw of String(md).split("\n")) {
    const line = raw.trimEnd();
    const bullet = /^\s*[-*+]\s+(.*)$/.exec(line);
    if (bullet) {
      if (!list) {
        list = document.createElement("ul");
        into.appendChild(list);
      }
      list.appendChild(text("li", bullet[1]));
      continue;
    }
    list = null;
    if (!line.trim()) continue;
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      // Offset by one: the pane already has an h3, and a document whose
      // headings outrank their own section is wrong for a screen reader.
      const level = Math.min(6, heading[1].length + 3);
      into.appendChild(text("h" + level, heading[2]));
    } else {
      into.appendChild(text("p", line));
    }
  }
}

function renderList(meetings) {
  clear(el.list);
  if (meetings.length === 0) {
    el.list.appendChild(text("p", "No meetings yet.", "empty"));
    return;
  }
  for (const m of meetings) {
    const item = document.createElement("button");
    item.className = "meeting";
    item.type = "button";
    item.appendChild(text("span", m.title || "Untitled meeting", "title"));
    item.appendChild(text("span", when(m.started_at_ms), "meta"));
    item.appendChild(text("span", listMeta(m), "meta"));
    item.addEventListener("click", () => openMeeting(m.id));
    el.list.appendChild(item);
  }
}

function renderDetail(detail) {
  clear(el.detail);
  el.detail.appendChild(text("h2", detail.meeting.title || "Untitled meeting"));
  el.detail.appendChild(text("p", when(detail.meeting.started_at_ms), "meta"));

  const actions = githubActions(detail.meeting.id);
  if (actions) el.detail.appendChild(actions);

  // Notes first, and above the summary, because they are the user's own words
  // and the summary is a derived artifact. Search has always indexed notes, so
  // before this they could be matched but never read.
  if (detail.note_md) {
    const notes = document.createElement("section");
    notes.className = "notes";
    notes.appendChild(text("h3", "Your notes"));
    const body = document.createElement("div");
    body.className = "note-body";
    renderMarkdown(detail.note_md, body);
    notes.appendChild(body);
    el.detail.appendChild(notes);
  }

  if (detail.summary_md) {
    const summary = document.createElement("section");
    summary.className = "summary";
    summary.appendChild(text("h3", "Summary"));
    const body = document.createElement("div");
    body.className = "summary-body";
    renderMarkdown(detail.summary_md, body);
    summary.appendChild(body);
    el.detail.appendChild(summary);
  } else {
    // The silent skip that was here is issue #74: "engine off", "engine
    // broken" and "engine fine" all rendered as the same blank space, and 33
    // meetings sat in that state without a word anywhere the user could see.
    const why = noSummaryReason(detail);
    if (why) {
      const section = document.createElement("section");
      section.className = "summary summary-missing";
      section.appendChild(text("h3", "Summary"));
      // Through `text()`, which sets textContent: `enrich_detail` carries an
      // engine subprocess's stderr, and that subprocess was just fed an
      // untrusted transcript. Nothing here builds markup from it (ING-11).
      section.appendChild(text("p", why, "empty"));
      el.detail.appendChild(section);
    }
  }

  const transcript = document.createElement("section");
  transcript.className = "transcript";
  transcript.appendChild(text("h3", "Transcript"));
  const body = document.createElement("div");
  body.id = "segments";
  let lastSpeaker = null;
  for (const seg of detail.segments) {
    body.appendChild(segmentRow(seg.channel, seg.start_ms, seg.speaker, seg.text, lastSpeaker));
    lastSpeaker = seg.speaker || lastSpeaker;
  }
  transcript.appendChild(body);
  el.detail.appendChild(transcript);
}

// Why this meeting has no summary, in words, or null to stay silent.
//
// Null for a meeting that has not been through enrichment yet (a null status)
// — a meeting that finished four seconds ago is not broken, and claiming it is
// would be its own kind of wrong.
function noSummaryReason(detail) {
  switch (detail.enrich_status) {
    case "no_engine":
      return "No summary — no summarization engine is configured. Turn one on under Summaries below.";
    case "engine_unresolvable":
      return (
        "No summary — the configured engine could not be found: " +
        (detail.enrich_detail || "unknown binary")
      );
    case "failed":
      return "Summary failed: " + (detail.enrich_detail || "no reason was recorded");
    default:
      return null;
  }
}

function renderHits(hits) {
  clear(el.list);
  if (hits.length === 0) {
    el.list.appendChild(text("p", "No matches.", "empty"));
    return;
  }
  for (const h of hits) {
    const item = document.createElement("button");
    item.className = "meeting";
    item.type = "button";
    item.appendChild(text("span", h.meeting_title || "Untitled meeting", "title"));
    item.appendChild(text("span", h.source, "meta"));
    item.appendChild(text("span", h.snippet, "snippet"));
    item.addEventListener("click", () => openMeeting(h.meeting_id));
    el.list.appendChild(item);
  }
}

function say(message) {
  el.status.textContent = message;
}

// One transcript row, identical for a stored segment and a live delta.
//
// The channel class is what tells the user's own lines from the far end's
// (#64) — capture keeps the legs on separate devices precisely so this is
// free, and the styling in app.css is where it becomes visible. The words
// live in a `.words` span because `.segment` is a three-column grid: a bare
// text node lands in the 3.5rem time column, which is how the live view
// shipped rendering two characters per line (#65).
function segmentRow(channel, startMs, speaker, words, lastSpeaker) {
  const line = document.createElement("p");
  line.className = "segment " + (channel || "");
  line.appendChild(text("span", offset(startMs), "at"));
  // Only when it changes. Repeating "S0" on ten consecutive lines is noise
  // that makes the actual turn-taking harder to see, not easier.
  if (speaker && speaker !== lastSpeaker) {
    line.appendChild(text("span", speaker, "speaker"));
  }
  line.appendChild(text("span", words, "words"));
  return line;
}

// ------------------------------------------------------------------ data

async function loadMeetings() {
  try {
    const body = await api("/api/meetings");
    renderList(body.meetings);
  } catch (e) {
    say("Could not load the library.");
  }
}

// The detail pane's current content, so a GitHub-settings change can redraw
// the per-meeting actions row without refetching. Cleared when the live view
// replaces the pane.
let currentDetail = null;

async function openMeeting(id) {
  try {
    const detail = await api("/api/meetings/" + encodeURIComponent(id));
    currentDetail = detail;
    renderDetail(detail);
  } catch (e) {
    say("Could not open that meeting.");
  }
}

// The library changed under us: refetch the list, and redraw the detail pane
// when the meeting that changed is the one being read (#78).
//
// The list refresh is guarded on an empty search box, and that guard is not
// optional: `renderList` and `renderHits` both open with `clear(el.list)`, so
// an unconditional refetch would silently replace search results someone is
// in the middle of reading. `onSearch` branches on the same condition.
//
// `meetingId` is omitted by the callers that know *something* changed but not
// what — a `resync` after backlog overflow, and the recording→idle poll edge.
// Those redraw whatever pane is open rather than nothing, because the frame
// they are standing in for would have named a meeting.
function refreshLibrary(meetingId) {
  if (el.search.value.trim() === "") loadMeetings();
  if (currentDetail && (!meetingId || currentDetail.meeting.id === meetingId)) {
    // Not `renderDetail(currentDetail)`: the point is the title and summary
    // that landed on the server since this pane was drawn.
    openMeeting(currentDetail.meeting.id);
  }
}

let searchTimer = null;
function onSearch() {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(async () => {
    const q = el.search.value.trim();
    if (q === "") {
      loadMeetings();
      return;
    }
    try {
      const body = await api("/api/search?q=" + encodeURIComponent(q));
      renderHits(body.hits);
    } catch (e) {
      say("Search failed.");
    }
  }, 150);
}

// ------------------------------------------------------- ING-07, live feed

// A browser cannot set Authorization on a WebSocket handshake -- the entire
// client API is `new WebSocket(url, protocols)`. So: an authenticated POST
// mints a ticket that is good for one connection within ten seconds, and the
// ticket travels in the query string because that is the only channel there
// is.
async function connectStream() {
  let ticket;
  try {
    const body = await api("/api/ws-ticket", { method: "POST" });
    ticket = body.ticket;
  } catch (e) {
    return;
  }

  const url = "ws://" + window.location.host + "/api/stream?ticket=" + encodeURIComponent(ticket);
  const socket = new WebSocket(url);

  socket.addEventListener("open", () => {
    el.live.hidden = false;
  });
  socket.addEventListener("message", (event) => {
    const frame = JSON.parse(event.data);
    if (frame.kind === "resync") {
      // Lag, not a library event: the client fell more than BACKLOG frames
      // behind and everything it missed is already in the store. A
      // meeting_ready lost in that overflow is recovered here, so the pane
      // gets the same redraw it would have got from the frame itself.
      refreshLibrary();
      return;
    }
    // #78: a meeting reached the library, or its title and summary landed.
    // Both mean the same thing on this side — what is on screen is older
    // than what the daemon has.
    if (frame.kind === "meeting_ready") {
      refreshLibrary(frame.meeting_id);
      return;
    }
    appendDeltas(frame.deltas || []);
  });
  socket.addEventListener("close", () => {
    el.live.hidden = true;
    // One ticket, one connection: reconnecting means minting another.
    setTimeout(connectStream, 2000);
  });
}

// Section 5.5 budgets ~50 rows in the DOM; a two-hour meeting is ~20k words.
const MAX_ROWS = 200;

function appendDeltas(deltas) {
  const body = document.getElementById("segments");
  if (!body) return;
  for (const d of deltas) {
    // Deltas carry no diarisation label; the channel is the truth here, and
    // "me" is what the mic leg means (§7.5). The far end's labels arrive
    // with the stored transcript after Stop.
    const speaker = d.channel === "mic" ? "me" : null;
    if (d.is_final === false) {
      // A revision, not a row: one in-progress line per channel, replaced on
      // every partial. An EMPTY partial is the server retracting the line —
      // sent when a mic final was suppressed as speaker echo, so the last
      // echo partial does not sit on screen as "me" for the rest of the
      // meeting.
      const id = "pending-" + d.channel;
      if (!d.text) {
        const old = document.getElementById(id);
        if (old) old.remove();
        continue;
      }
      const fresh = segmentRow(d.channel, d.start_ms, speaker, d.text, null);
      fresh.id = id;
      fresh.classList.add("pending");
      const old = document.getElementById(id);
      if (old) old.replaceWith(fresh);
      else body.appendChild(fresh);
      continue;
    }
    const pending = document.getElementById("pending-" + d.channel);
    if (pending) pending.remove();
    body.appendChild(segmentRow(d.channel, d.start_ms, speaker, d.text, null));
  }
  while (body.childElementCount > MAX_ROWS) {
    body.removeChild(body.firstChild);
  }
}

// -------------------------------------------------- CON-01, recording state

// How often to re-read the recorder while the tab is open.
//
// Polled rather than pushed: /api/stream is one-directional by design and its
// hub coalesces transcript deltas, dropping a flush when the buffer is empty,
// so a state change has nothing to ride on. Five seconds is slow enough to be
// invisible on the wire and fast enough that a recording started from the
// menu bar shows up here before anyone wonders.
const RECORDING_POLL_MS = 5000;

// How often to re-read while the meeting is being written (#77).
//
// Finishing is short and the user is waiting on it — a five-second gap between
// "Finishing…" and "Saved" reads as a hang. This rate applies only inside that
// window; the base poll above is what runs the rest of the time.
const FINISHING_POLL_MS = 1000;

// The daemon's own word: "idle", "recording" or "finishing". One value drives
// the badge, the button, the clock and the poll rate, so nothing can disagree
// with anything else.
let recState = "idle";
let recordingSince = null;
let clockTimer = null;
let finishingTimer = null;
// How long the last meeting ran. Kept after the state falls back to idle: the
// clock used to go from ticking to gone, so the one number a user wants at the
// end — how long that was — was the one they never saw.
let lastFinalMs = null;

function formatElapsed(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? h + ":" + String(m).padStart(2, "0") + ":" + sec : m + ":" + sec;
}

// Ticks locally every second from the last started_at_ms the poll reported.
// The poll corrects drift; the local tick is what makes it a clock rather
// than a number that jumps every five seconds.
function paintClock() {
  if (!recordingSince) return;
  el.elapsed.textContent = formatElapsed(Date.now() - recordingSince);
}

function stopClock() {
  if (clockTimer) {
    clearInterval(clockTimer);
    clockTimer = null;
  }
}

// One extra timer, never two. `setInterval` here is fixed-rate, so starting one
// per poll that saw "finishing" would stack them, and each survives the state
// that created it.
function startFinishingPoll() {
  if (!finishingTimer) finishingTimer = setInterval(pollRecording, FINISHING_POLL_MS);
}

function stopFinishingPoll() {
  if (finishingTimer) {
    clearInterval(finishingTimer);
    finishingTimer = null;
  }
}

// A daemon with no recorder answers 404 on these paths, which is
// indistinguishable from a server that never had them. Once we learn that,
// stop asking and leave the controls hidden.
let recorderPresent = true;

async function pollRecording() {
  if (!recorderPresent) return;
  try {
    renderRecording(await api("/api/recording/status"));
  } catch (e) {
    // Not "the request failed" — this build has no recorder at all.
    recorderPresent = false;
    showRecordingControls(false);
  }
}

function showRecordingControls(visible) {
  el.record.hidden = !visible;
  el.consentLabel.hidden = !visible;
  if (!visible) {
    el.recording.hidden = true;
    // Nothing left to poll for: a build with no recorder answers 404 forever.
    stopFinishingPoll();
  }
}

// The one place the button's label and enabled state are decided, so the
// consent handler cannot contradict the poll.
//
// CON-01 lives in the last branch: Start is enabled only by a ticked box. The
// other two are what #77 got wrong — a box ticked *during* finishing used to
// re-enable a button with nothing to do, and Stop must never be gated on the
// box, because a user who cannot stop a recording is the worse failure.
function paintRecordButton() {
  el.record.textContent = recState === "idle" ? "Start" : "Stop";
  if (recState === "finishing") el.record.disabled = true;
  else if (recState === "recording") el.record.disabled = false;
  else el.record.disabled = !el.consent.checked;
}

function renderRecording(body) {
  const was = recState;
  // Anything the daemon does not name is treated as idle. An unrecognised
  // word must never be the one that leaves a clock running.
  recState =
    body.state === "recording" || body.state === "finishing" ? body.state : "idle";
  showRecordingControls(true);

  el.recording.hidden = recState === "idle";
  // The badge is static markup ("recording", index.html), so the finishing
  // word has to be written *and* written back, or it sticks for the rest of
  // the tab's life and the next meeting records under a "finishing…" label.
  // ING-11: textContent, as everything in this file writes text.
  el.recording.textContent = recState === "finishing" ? "finishing…" : "recording";

  // The session clock (#66 follow-up): a recording with no visible duration
  // is how a 37-minute accident happens. Exhaustive on purpose — a
  // "recording" with a null started_at_ms used to fall through both branches
  // and leave the timer ticking on the previous session's start.
  if (recState === "recording" && body.started_at_ms) {
    recordingSince = body.started_at_ms;
    lastFinalMs = null;
    el.elapsed.hidden = false;
    paintClock();
    if (!clockTimer) clockTimer = setInterval(paintClock, 1000);
  } else {
    // Frozen, not hidden. The clock stopped with capture, and the number the
    // meeting ended on is the one the user is looking for (#77) — hiding it
    // is why the duration went from ticking straight to gone. The server's
    // own elapsed_ms is the source: it froze it, we only draw it.
    recordingSince = null;
    stopClock();
    if (recState === "finishing" && typeof body.elapsed_ms === "number") {
      lastFinalMs = body.elapsed_ms;
    }
    el.elapsed.hidden = lastFinalMs === null;
    if (lastFinalMs !== null) el.elapsed.textContent = formatElapsed(lastFinalMs);
  }

  // The tick box is the control CON-01 asks for, so it is only meaningful
  // before a recording exists.
  el.consentLabel.hidden = recState !== "idle";
  paintRecordButton();

  // finishing → idle is the moment the meeting reached the library, and the
  // moment to say so.
  if (was === "finishing" && recState === "idle") {
    say(lastFinalMs === null ? "Saved to your library." : "Saved — " + formatElapsed(lastFinalMs) + ".");
  }
  // Belt and braces for #78. The `meeting_ready` frame is what normally
  // refreshes the library; this recovers a tab that never received it — one
  // whose socket was between reconnects, or whose frame was lost to backlog
  // overflow with no `resync` reaching it either. The edge is leaving *any*
  // live state for idle rather than finishing→idle alone, because a poll can
  // straddle a whole finish and only ever observe recording→idle.
  if (was !== "idle" && recState === "idle") {
    refreshLibrary();
  }
  if (recState === "finishing") startFinishingPoll();
  else stopFinishingPoll();

  // Said out loud while the meeting is still running. Two Deepgram bugs each
  // killed the stream on connect and reported nothing anywhere, so hours of
  // audio were captured beside an empty transcript that looked exactly like a
  // quiet meeting. Finding out afterwards is finding out too late.
  // Kept accurate for finishing too: the daemon still reports the failure
  // there, and "Recording, but…" over a closed microphone is the same class of
  // lie #77 is about.
  if (body.transcription_error) {
    say(
      (recState === "finishing"
        ? "Transcription failed during this meeting: "
        : "Recording, but transcription is failing: ") + body.transcription_error,
    );
  }
}

// Give the live transcript somewhere to land. `appendDeltas` targets
// `#segments`, which until now existed only inside an opened meeting's detail
// view — so live words had a working socket and no element to render into.
// Only called from the Start click, never from the poll: replacing the detail
// pane is acceptable as the direct result of an action, and rude as a side
// effect of a timer while someone is reading an old meeting.
function showLive() {
  currentDetail = null;
  clear(el.detail);
  el.detail.appendChild(text("h2", "Recording"));
  el.detail.appendChild(
    text("p", "Words appear here as the provider finalizes them.", "meta"),
  );
  const transcript = document.createElement("section");
  transcript.className = "transcript";
  const body = document.createElement("div");
  body.id = "segments";
  transcript.appendChild(body);
  el.detail.appendChild(transcript);
}

async function onRecord() {
  // Nothing to press while the meeting is being written: capture is already
  // over and Start is refused until the slot frees. The button is disabled in
  // that state, so this only catches a click that raced the poll.
  if (recState === "finishing") return;
  const path =
    recState === "recording"
      ? "/api/recording/stop"
      : "/api/recording/start?ack=all-party";
  el.record.disabled = true;
  try {
    const body = await api(path, { method: "POST" });
    renderRecording(body);
    if (body.error === "consent_required") {
      say("Tick the box first: everyone on the call has to have consented.");
    } else if (body.error) {
      say("The recorder said: " + body.error);
    } else if (body.state === "recording") {
      say("Recording. Tell the other participants.");
      showLive();
    } else if (body.state === "finishing") {
      // Stop branches on the *answer*, not on what was asked. It used to fall
      // into the start branch, announce "Recording. Tell the other
      // participants." and call showLive(), which wiped the pane holding the
      // meeting's own transcript (#77).
      say("Stopped. Finishing — writing the meeting to your library.");
    } else {
      say("Stopped. The meeting is being written to your library.");
      await loadMeetings();
    }
  } catch (e) {
    say("Could not reach the recorder.");
    // Not a flat `disabled = false`: CON-01 says an idle Start stays disabled
    // until the box is ticked.
    paintRecordButton();
  }
}

// ------------------------------------------- issue #63, GitHub export

// The same "404 means the feature is absent" convention the recorder uses: a
// read-only build hides the section entirely rather than showing dead knobs.
let githubPresent = true;
let githubSettings = null;

// The stable machine codes from the API, spelled for a person. Anything not
// listed renders verbatim — GitHub's own error text beats a shrug.
const GH_ERRORS = {
  gh_missing: "The gh CLI is not installed. brew install gh, then try again.",
  gh_not_authenticated: "gh has no login. Run gh auth login in a terminal, then try again.",
  repo_not_found: "That repository is not reachable with your gh login. Check the name and your access.",
  github_export_disabled: "GitHub export is switched off. Enable it in the GitHub export section first.",
};

function ghExplain(code) {
  return GH_ERRORS[code] || code;
}

function renderGithubForm(s) {
  githubSettings = s;
  el.ghRepo.value = s.repo || "";
  el.ghBranch.value = s.branch || "";
  el.ghPrefix.value = s.path_prefix || "";
  el.ghAuto.checked = s.mode === "auto";
  el.ghEnabled.checked = Boolean(s.enabled);
  el.ghSettings.hidden = false;
}

async function loadGithub() {
  if (!githubPresent) return;
  try {
    const body = await api("/api/settings/github");
    renderGithubForm(body.settings);
  } catch (e) {
    // Not "the request failed" — this build has no GitHub export at all.
    githubPresent = false;
    el.ghSettings.hidden = true;
  }
}

// Fetched once, when the section is first opened: listing repos runs the gh
// CLI on the daemon side, and page load should not pay for a picker nobody
// opened. A failure is said out loud and retried on the next open — the
// field itself keeps working as free text either way.
let ghReposLoaded = false;

async function loadGithubRepos() {
  if (ghReposLoaded) return;
  try {
    const body = await api("/api/settings/github/repos");
    if (body.error) {
      say(ghExplain(body.error));
      return;
    }
    clear(el.ghRepoList);
    for (const name of body.repos) {
      const option = document.createElement("option");
      option.value = name;
      el.ghRepoList.appendChild(option);
    }
    ghReposLoaded = true;
  } catch (e) {
    // The control is absent or the request failed; the field stays free text.
  }
}

async function onGithubSave() {
  el.ghSave.disabled = true;
  try {
    const body = await api("/api/settings/github", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        enabled: el.ghEnabled.checked,
        repo: el.ghRepo.value,
        branch: el.ghBranch.value,
        path_prefix: el.ghPrefix.value,
        mode: el.ghAuto.checked ? "auto" : "manual",
      }),
    });
    if (body.error) {
      // The form keeps what the user typed — wiping it back to the stored
      // values would make them re-enter everything to fix one field.
      say("Not saved. " + body.error.replace("invalid_settings: ", ""));
      el.ghSave.disabled = false;
      return;
    }
    // Accepted: re-render from the reply (the normalized spelling), and
    // redraw the open meeting so its push button appears or disappears with
    // the setting it depends on.
    renderGithubForm(body.settings);
    if (currentDetail) renderDetail(currentDetail);
    if (body.settings.enabled && body.settings.mode === "auto") {
      say("Saved. New meetings will be pushed to " + body.settings.repo + " when they finish.");
    } else if (body.settings.enabled) {
      say("Saved. Use the button on a meeting to push it to " + body.settings.repo + ".");
    } else {
      say("Saved. GitHub export is off.");
    }
  } catch (e) {
    say("Could not save the GitHub settings.");
  }
  el.ghSave.disabled = false;
}

// ------------------------------------------- issue #74, summarization engine

// Same "404 means the feature is absent" convention as the GitHub section.
let summarizePresent = true;
let summarizeDisclosures = {};

function renderSummarizeForm(body) {
  const s = body.settings;
  el.sumKind.value = s.cli_kind || "claude";
  el.sumBinary.value = s.binary || "";
  el.sumAck.checked = Boolean(s.acknowledged_egress);
  el.sumEnabled.checked = Boolean(s.cli_enabled);
  summarizeDisclosures = body.status.disclosures || {};
  paintDisclosure();
  el.sumStatus.textContent = summarizeStatusLine(body.status);
  el.sumSettings.hidden = false;
}

// KEY-04's own words, from the daemon, for the engine the picker is on right
// now — not for the one that happens to be stored. A checkbox beside the
// wrong disclosure collects an acknowledgement of facts that are not true of
// what is about to be saved.
function paintDisclosure() {
  const lines = summarizeDisclosures[el.sumKind.value] || [];
  el.sumDisclosure.textContent = lines.join(" ");
}

// What the daemon actually resolves — the diagnostic that cannot lie, since it
// is the daemon's own answer and not the shell's.
function summarizeStatusLine(status) {
  if (status.engine === "anthropic") {
    return "Engine: the Anthropic API (a key is in your keychain, and it wins over any CLI).";
  }
  if (status.engine === "none") {
    if (status.configured_binary && !status.binary_resolves) {
      return (
        "Engine: none. `" +
        status.configured_binary +
        "` is configured, but this daemon cannot find it. Enter a full path above."
      );
    }
    return "Engine: none. Finished meetings get a title but no summary.";
  }
  let line = "Engine: " + status.engine + " (" + (status.resolved_binary || "") + ")";
  // Both, whenever they differ: a status that named only the configured
  // spelling would describe a binary the daemon is not running.
  if (status.resolved_binary && status.resolved_binary !== status.configured_binary) {
    line += ", configured as `" + status.configured_binary + "`";
  }
  return line + ".";
}

async function loadSummarize() {
  if (!summarizePresent) return;
  try {
    renderSummarizeForm(await api("/api/settings/summarize"));
  } catch (e) {
    // Not "the request failed" — this build has no engine control at all.
    summarizePresent = false;
    el.sumSettings.hidden = true;
  }
}

async function onSummarizeSave() {
  el.sumSave.disabled = true;
  try {
    const body = await api("/api/settings/summarize", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        cli_enabled: el.sumEnabled.checked,
        acknowledged_egress: el.sumAck.checked,
        cli_kind: el.sumKind.value,
        binary: el.sumBinary.value,
      }),
    });
    if (body.error === "disclosure_required") {
      // KEY-04 is enforced by the API, not by this checkbox — the box is the
      // affordance, the refusal is the control.
      say("Not saved. Tick the box acknowledging that transcripts leave this machine.");
      el.sumSave.disabled = false;
      return;
    }
    if (body.error) {
      say("Not saved. " + body.error.replace("invalid_settings: ", ""));
      el.sumSave.disabled = false;
      return;
    }
    renderSummarizeForm(body);
    // The open meeting's "no summary" line depends on the engine, so redraw.
    if (currentDetail) renderDetail(currentDetail);
    if (body.settings.cli_enabled && body.status.binary_resolves) {
      say("Saved. Finished meetings now get a summary and action items.");
    } else if (body.settings.cli_enabled) {
      // Saved but inert: say so rather than promising summaries that will not
      // appear — the exact silence #74 is about.
      say("Saved, but `" + body.status.configured_binary + "` does not resolve here. Enter a full path.");
    } else {
      say("Saved. Summaries are off.");
    }
  } catch (e) {
    say("Could not save the summarization settings.");
  }
  el.sumSave.disabled = false;
}

async function onGithubPush(meetingId, button) {
  button.disabled = true;
  say("Pushing to " + (githubSettings ? githubSettings.repo : "GitHub") + "…");
  try {
    const body = await api(
      "/api/meetings/" + encodeURIComponent(meetingId) + "/github-push",
      { method: "POST" },
    );
    if (body.error) {
      say(ghExplain(body.error));
    } else {
      say("Pushed: " + body.receipt.repo + "/" + body.receipt.path);
    }
  } catch (e) {
    say("Could not push that meeting.");
  }
  button.disabled = false;
}

// The per-meeting button, appended into the detail pane when the feature is
// present and switched on.
function githubActions(meetingId) {
  if (!githubPresent || !githubSettings || !githubSettings.enabled) return null;
  const row = document.createElement("p");
  row.className = "gh-actions";
  const button = document.createElement("button");
  button.type = "button";
  button.className = "gh-push";
  button.textContent = "Push to GitHub";
  button.addEventListener("click", () => onGithubPush(meetingId, button));
  row.appendChild(button);
  return row;
}

// ------------------------------------------------------------------ start

async function main() {
  await redeemHandoff();
  if (!token()) {
    say("This tab is not authorised. Click the FlyOnTheWall app icon to open a fresh one.");
    return;
  }
  el.search.addEventListener("input", onSearch);
  el.record.addEventListener("click", onRecord);
  // Re-evaluates the disabled state; the box gates Start and nothing else.
  // Through `paintRecordButton` so it is state-aware: ticking the box while a
  // meeting is being written must not re-enable a button with nothing to do.
  el.consent.addEventListener("change", paintRecordButton);
  el.ghSave.addEventListener("click", onGithubSave);
  el.sumSave.addEventListener("click", onSummarizeSave);
  // The disclosure differs per engine, so it follows the picker rather than
  // waiting for a save.
  el.sumKind.addEventListener("change", paintDisclosure);
  el.ghSettings.addEventListener("toggle", function () {
    if (el.ghSettings.open) loadGithubRepos();
  });
  await loadMeetings();
  await loadGithub();
  await loadSummarize();
  await pollRecording();
  setInterval(pollRecording, RECORDING_POLL_MS);
  connectStream();
}

main();

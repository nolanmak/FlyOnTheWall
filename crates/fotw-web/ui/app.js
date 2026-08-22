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
      loadMeetings();
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

let recordingNow = false;

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
  if (!visible) el.recording.hidden = true;
}

function renderRecording(body) {
  recordingNow = body.state === "recording";
  showRecordingControls(true);

  el.recording.hidden = !recordingNow;
  el.record.textContent = recordingNow ? "Stop" : "Start";

  // The tick box is the control CON-01 asks for, so it is only meaningful
  // before a recording exists. While one is running, Stop must never be
  // gated on it — a user who cannot stop a recording is the worse failure.
  el.consentLabel.hidden = recordingNow;
  el.record.disabled = !recordingNow && !el.consent.checked;

  // Said out loud while the meeting is still running. Two Deepgram bugs each
  // killed the stream on connect and reported nothing anywhere, so hours of
  // audio were captured beside an empty transcript that looked exactly like a
  // quiet meeting. Finding out afterwards is finding out too late.
  if (body.transcription_error) {
    say("Recording, but transcription is failing: " + body.transcription_error);
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
  const path = recordingNow
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
    } else {
      say("Stopped. The meeting is being written to your library.");
      await loadMeetings();
    }
  } catch (e) {
    say("Could not reach the recorder.");
    el.record.disabled = false;
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
  el.consent.addEventListener("change", function () {
    el.record.disabled = !recordingNow && !el.consent.checked;
  });
  el.ghSave.addEventListener("click", onGithubSave);
  el.ghSettings.addEventListener("toggle", function () {
    if (el.ghSettings.open) loadGithubRepos();
  });
  await loadMeetings();
  await loadGithub();
  await pollRecording();
  setInterval(pollRecording, RECORDING_POLL_MS);
  connectStream();
}

main();

// The whole client. Plain DOM, no framework, no build step.
//
// Two rules, both from docs/REQUIREMENTS.md 10.1, and both of which the Rust
// side has a test for:
//
//   ING-08  The bearer token lives in sessionStorage and nowhere else. Never a
//           cookie: RFC 6265 scopes cookies by *host*, so a cookie set by
//           127.0.0.1:51234 would be sent to every other service on every
//           other port of 127.0.0.1 -- every local dev server, every other
//           app's helper process. sessionStorage is keyed by the full origin
//           including the port, and it is not ambient: nothing attaches it to
//           a request unless this file does.
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
  sessionStorage.setItem(TOKEN_KEY, body.token);
}

function token() {
  return sessionStorage.getItem(TOKEN_KEY);
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
    const line = document.createElement("p");
    line.className = "segment";
    line.appendChild(text("span", offset(seg.start_ms), "at"));
    // Only when it changes. Repeating "S0" on ten consecutive lines is noise
    // that makes the actual turn-taking harder to see, not easier.
    if (seg.speaker && seg.speaker !== lastSpeaker) {
      line.appendChild(text("span", seg.speaker, "speaker"));
    }
    lastSpeaker = seg.speaker || lastSpeaker;
    line.appendChild(text("span", seg.text, "words"));
    body.appendChild(line);
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

// ------------------------------------------------------------------ data

async function loadMeetings() {
  try {
    const body = await api("/api/meetings");
    renderList(body.meetings);
  } catch (e) {
    say("Could not load the library.");
  }
}

async function openMeeting(id) {
  try {
    const detail = await api("/api/meetings/" + encodeURIComponent(id));
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
    body.appendChild(text("p", d.text, "segment " + d.channel));
  }
  while (body.childElementCount > MAX_ROWS) {
    body.removeChild(body.firstChild);
  }
}

// ------------------------------------------------------------------ start

async function main() {
  await redeemHandoff();
  if (!token()) {
    say("This tab is not authorised. Reopen FlyOnTheWall from the menu bar.");
    return;
  }
  el.search.addEventListener("input", onSearch);
  await loadMeetings();
  connectStream();
}

main();

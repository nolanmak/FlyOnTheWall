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
// Native Settings opens one named section. Only these content-free anchors may
// survive handoff redemption; credentials and arbitrary fragments are removed.
const REQUESTED_SETTINGS = ["#speech-settings", "#sum-settings"].includes(window.location.hash)
  ? window.location.hash : "";
function openRequestedSettings() {
  const target = REQUESTED_SETTINGS === "#speech-settings" ? el.speechSettings
    : REQUESTED_SETTINGS === "#sum-settings" ? el.sumSettings : null;
  if (!target || target.hidden) return false;
  // Every section lives inside the collapsed Settings panel.
  if (el.settingsPanel) el.settingsPanel.open = true;
  target.open = true;
  target.scrollIntoView?.({block: "start", behavior: "auto"});
  return true;
}

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
  returnLive: document.getElementById("return-live"),
  noteDrafts: document.getElementById("note-drafts"),
  recording: document.getElementById("recording"),
  recordingLimit: document.getElementById("recording-limit"),
  captureHealth: document.getElementById("capture-health"),
  speechHealth: document.getElementById("speech-health"),
  liveStatus: document.getElementById("live-status"),
  liveStatusSummary: document.getElementById("live-status-summary"),
  settingsPanel: document.getElementById("settings-panel"),
  ghSettings: document.getElementById("gh-settings"),
  ghRepo: document.getElementById("gh-repo"),
  ghBranch: document.getElementById("gh-branch"),
  ghPrefix: document.getElementById("gh-prefix"),
  ghAuto: document.getElementById("gh-auto"),
  ghEnabled: document.getElementById("gh-enabled"),
  ghSave: document.getElementById("gh-save"),
  ghRepoList: document.getElementById("gh-repo-list"),
  ghPublic: document.getElementById("gh-public"),
  ghWholeLibrary: document.getElementById("gh-whole-library"),
  captureSettings: document.getElementById("capture-settings"),
  captureSummary: document.getElementById("capture-summary"),
  captureSpotify: document.getElementById("capture-spotify"),
  captureBrave: document.getElementById("capture-brave"),
  captureSave: document.getElementById("capture-save"),
  captureReload: document.getElementById("capture-reload"),
  captureStatus: document.getElementById("capture-status"),
  captureDisclosure: document.getElementById("capture-disclosure"),
  speechSettings: document.getElementById("speech-settings"),
  speechMode: document.getElementById("speech-mode"),
  speechModel: document.getElementById("speech-model"),
  speechLanguage: document.getElementById("speech-language"),
  speechCaptions: document.getElementById("speech-captions"),
  speechCaptionsNote: document.getElementById("speech-captions-note"),
  speechDisclosure: document.getElementById("speech-disclosure"),
  speechLocalFields: document.getElementById("speech-local-fields"),
  speechSave: document.getElementById("speech-save"),
  speechReload: document.getElementById("speech-reload"),
  speechStatus: document.getElementById("speech-status"),
  sumSettings: document.getElementById("sum-settings"),
  sumKind: document.getElementById("sum-kind"),
  sumBinary: document.getElementById("sum-binary"),
  sumDisclosure: document.getElementById("sum-disclosure"),
  sumAck: document.getElementById("sum-ack"),
  sumAckRow: document.getElementById("sum-ack-row"),
  sumBinaryRow: document.getElementById("sum-binary-row"),
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
  history.replaceState(null, "", window.location.pathname + REQUESTED_SETTINGS);

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
  if (!res.ok) {
    // The message stays what every caller already switches on. The status rides
    // beside it because "this build has no such control" and "the request
    // failed" are different facts and only the code tells them apart: a 404 is
    // the convention the recorder and the settings form use for an absent
    // feature, while a 500, an expired token or a daemon mid-restart is a
    // problem worth reporting. github.js draws a different thing for each
    // (#112); the rest ignore the extra property.
    const failed = new Error("request failed");
    failed.status = res.status;
    try { failed.code = (await res.json()).error; } catch (_) {}
    throw failed;
  }
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
  if (typeof FOTWNotes !== "undefined") FOTWNotes.release?.(node);
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
// A deliberately small subset: headings, bullets, paragraphs and blockquotes.
// It builds DOM nodes and assigns textContent, exactly like every other
// renderer here, so ING-11 still holds -- a summary is model output written
// over attacker-influenced transcript, and handing that to a markdown library
// that emits HTML is precisely how a transcript becomes script. Rendering it
// as preformatted text was safe but showed users literal "## " on every
// heading.
//
// Anything unrecognised falls through as a paragraph, verbatim. That is the
// right failure: unstyled real text beats swallowed text.

// The admonition markers the daemon actually emits, and what each one is
// called on screen. Both the class and the label are read out of this table;
// neither is ever built from the parsed token. A summary is model output over
// an untrusted transcript, so a marker-shaped string somewhere in the prose
// must not be able to name a CSS class or write its own heading -- an
// unrecognised `> [!ANYTHING]` stays ordinary quoted text, which is the same
// failure direction as the paragraph fallback above.
//
// A Map rather than an object literal because a lookup keyed by untrusted text
// must not be able to find `constructor` or `toString` on a prototype.
//
// Only [!WARNING] is emitted today, by both of them -- #75's failed extraction
// and #84's low-grounding banner. [!NOTE] is here because GitHub's other
// common marker costs one line and one CSS rule, and a future warning worded
// as an aside should not have to reopen this.
const ADMONITIONS = new Map([
  ["[!WARNING]", { className: "callout callout-warning", label: "Warning" }],
  ["[!NOTE]", { className: "callout callout-note", label: "Note" }],
]);

function renderMarkdown(md, into, headingBase = 3) {
  let list = null;
  let quote = null;
  for (const raw of String(md).split("\n")) {
    const line = raw.trimEnd();
    const bullet = /^\s*[-*+]\s+(.*)$/.exec(line);
    if (bullet) {
      quote = null;
      if (!list) {
        list = document.createElement("ul");
        into.appendChild(list);
      }
      list.appendChild(text("li", bullet[1]));
      continue;
    }
    list = null;
    // #90. The daemon writes its run warnings into `body_md` as a two-line
    // `> [!WARNING]` admonition, and #84 puts one of them above the prose --
    // so the first thing in the pane was a line of markdown source. Grouping
    // *consecutive* quoted lines matters: rendered one at a time, the marker
    // and the sentence it labels are two unrelated blocks.
    const quoted = /^>\s?(.*)$/.exec(line);
    if (quoted) {
      const body = quoted[1];
      if (!quote) {
        quote = document.createElement("blockquote");
        into.appendChild(quote);
        const marker = ADMONITIONS.get(body.trim());
        if (marker) {
          quote.className = marker.className;
          // A real text node rather than a CSS `content:` label, so the word
          // is selectable, copies with the callout, and is announced.
          quote.appendChild(text("p", marker.label, "callout-label"));
          continue;
        }
      }
      // A bare `>` continues the quote without contributing an empty
      // paragraph to it.
      if (body.trim()) quote.appendChild(text("p", body));
      continue;
    }
    quote = null;
    if (!line.trim()) continue;
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      // Offset by the caller's base. The pane takes the default 3, because a
      // summary sits under the section's own h3 and a document whose headings
      // outrank their own section is wrong for a screen reader. The clipboard
      // passes 1: a paste is its own document under an h1 of the title, and
      // Slack, Notion and Docs map h2/h3 to real heading blocks.
      const level = Math.min(6, heading[1].length + headingBase);
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
    item.dataset.meetingId = m.id;
    item.setAttribute("aria-current", String(m.id === selectedMeetingId));
    item.appendChild(text("span", m.title || "Untitled meeting", "title"));
    item.appendChild(text("span", when(m.started_at_ms), "meta"));
    item.appendChild(text("span", listMeta(m), "meta"));
    item.addEventListener("click", () => openMeeting(m.id));
    el.list.appendChild(item);
  }
}

// The meeting title, with an inline rename. The title is the one name the whole
// app reads from: this list, the detail header here, and the sharing document's
// own heading and download filename. So a rename typed here is also how two
// calls with the same client stop sharing one generic document name. The daemon
// is the source of truth and validates the input; a human title then survives
// enrichment untouched (the `replaceable` guard), so no draft is regenerated.
function renderTitleHeader(detail) {
  const row = document.createElement("div");
  row.className = "detail-title";
  const heading = text("h2", detail.meeting.title || "Untitled meeting");
  const rename = document.createElement("button");
  rename.type = "button";
  rename.className = "rename";
  rename.textContent = "Rename";
  // Out of context "Rename" says nothing about what — the pane has several
  // controls — so the accessible name carries the object (WCAG 2.5.3).
  rename.setAttribute("aria-label", "Rename this meeting");
  rename.addEventListener("click", () => beginRename(detail, row, heading));
  row.appendChild(heading);
  row.appendChild(rename);
  return row;
}

// Swap the static title for an input and Save/Cancel, restoring the header on
// either outcome. Enter saves, Escape cancels.
function beginRename(detail, row, heading) {
  const current = detail.meeting.title || "";
  clear(row);
  const input = document.createElement("input");
  input.type = "text";
  input.className = "rename-input";
  input.value = current;
  // Matches the daemon's MAX_TITLE_CHARS; the server re-checks regardless.
  input.maxLength = 100;
  input.setAttribute("aria-label", "Meeting name");
  const save = document.createElement("button");
  save.type = "button";
  save.className = "rename-save";
  save.textContent = "Save";
  save.setAttribute("aria-label", "Save the meeting name");
  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "rename-cancel";
  cancel.textContent = "Cancel";
  cancel.setAttribute("aria-label", "Cancel renaming");

  const restore = () => {
    clear(row);
    row.appendChild(heading);
    const again = document.createElement("button");
    again.type = "button";
    again.className = "rename";
    again.textContent = "Rename";
    again.setAttribute("aria-label", "Rename this meeting");
    again.addEventListener("click", () => beginRename(detail, row, heading));
    row.appendChild(again);
  };

  let busy = false;
  const commit = async () => {
    if (busy) return;
    const title = input.value.trim();
    if (title === "") {
      say("Enter a name for this meeting.");
      input.focus();
      return;
    }
    if (title === current) {
      restore();
      return;
    }
    busy = true;
    save.disabled = true;
    cancel.disabled = true;
    try {
      const body = await api(
        "/api/meetings/" + encodeURIComponent(detail.meeting.id) + "/document",
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ action: "rename", title }),
        },
      );
      if (body.error) {
        // The daemon refused it (too long, empty once normalised). Stay in the
        // editor so the typed name is not lost.
        say(body.error);
        busy = false;
        save.disabled = false;
        cancel.disabled = false;
        input.focus();
        return;
      }
      say("Renamed.");
      // The title feeds both panes: redraw this meeting and refresh the list so
      // the left menu shows the new name without a reload.
      openMeeting(detail.meeting.id);
      loadMeetings();
    } catch (e) {
      say("Could not rename that meeting.");
      busy = false;
      save.disabled = false;
      cancel.disabled = false;
    }
  };

  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      commit();
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      restore();
    }
  });
  save.addEventListener("click", commit);
  cancel.addEventListener("click", restore);
  row.appendChild(input);
  row.appendChild(save);
  row.appendChild(cancel);
  input.focus();
  input.select();
}

function clearDetail() {
  if (typeof releaseSharingDocument === "function") releaseSharingDocument();
  clear(el.detail);
}

function renderDetail(detail) {
  clearDetail();
  el.detail.appendChild(renderTitleHeader(detail));
  el.detail.appendChild(text("p", when(detail.meeting.started_at_ms), "meta"));

  const actions = actionsRow(detail);
  if (actions) el.detail.appendChild(actions);

  // Where this meeting stands with GitHub, in place of the push button #112
  // removed. Draws nothing at all when export is off or absent.
  mountGithubSync(detail, el.detail);

  mountSharingDocument(detail, el.detail);

  if (typeof FOTWSpeechRecovery !== "undefined") {
    FOTWSpeechRecovery.mount(detail.meeting.id, el.detail, api, () => openMeeting(detail.meeting.id));
  }

  // Notes first, and above the summary, because they are the user's own words
  // and the summary is a derived artifact. Search has always indexed notes, so
  // before this they could be matched but never read.
  if (typeof FOTWNotes !== "undefined") {
    FOTWNotes.mount(detail.meeting.id, el.detail, api);
  } else if (detail.note_md) {
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
  transcript.appendChild(renderSavedSpeechProvenance(detail.speech_provenance));
  transcript.appendChild(text("p", "Time since first speech", "meta"));
  const body = document.createElement("div");
  body.id = "segments";
  mountSavedTranscript(transcript, body, detail.segments);
  transcript.appendChild(body);
  el.detail.appendChild(transcript);
  paintLiveNavigation();
}

// Recorded engine history is independent of the next recording's settings.
// Receipt boundaries use original audio samples; they never shift to first speech.
function renderSavedSpeechProvenance(history) {
  const section = text("section", "", "saved-speech");
  if (!Array.isArray(history) || history.length === 0) {
    section.appendChild(text("p", "Engine history was not recorded for this transcript.", "meta"));
    return section;
  }
  const details = document.createElement("details");
  details.appendChild(text("summary", "Transcription sources"));
  details.appendChild(text("p", "Times use the original recording clock. Processed audio does not guarantee a complete or accurate transcript.", "meta"));
  const reasons = {transport_budget: "network retry budget exhausted", terminal_provider: "provider refused the request", provider_configuration: "provider configuration failed", replay_gap: "cloud replay coverage unavailable", manual: "manual provider change"};
  for (const epoch of history) {
    const row = text("article", "", "speech-source");
    const label = epoch.source === "mic" ? "Microphone" : "System audio";
    row.appendChild(text("strong", label + " · " + speechProviderName(epoch.provider) + " / " + String(epoch.model || "Unknown model"), "speech-engine"));
    const from = formatElapsed(Math.floor(epoch.from_sample / 16));
    row.appendChild(text("p", typeof epoch.through_sample === "number"
      ? "Audio processed " + from + "–" + formatElapsed(Math.floor(epoch.through_sample / 16))
      : "Engine selected at " + from + "; no confirmed processed audio."));
    if (epoch.reason) row.appendChild(text("p", reasons[epoch.reason] || "Provider changed"));
    const caps = epoch.capabilities || {};
    if (caps.diarization === "none") row.appendChild(text("p", "Speaker labels unavailable"));
    if (caps.wordTimestamps === "none") row.appendChild(text("p", "Word timing unavailable"));
    if (epoch.epoch > 0 && epoch.source === "system") row.appendChild(text("p", "Speaker identities are separate for each engine."));
    row.appendChild(text("p", epoch.revision ? "Model revision: " + epoch.revision : "Model revision not recorded", "meta"));
    details.appendChild(row);
  }
  section.appendChild(details);
  return section;
}
function speechProviderName(provider) {
  return new Map([["deepgram", "Deepgram"], ["whispercpp", "Whisper"], ["local-whisper", "Whisper"], ["whisper.cpp", "Whisper"], ["fotw-speechd", "FluidAudio"]]).get(provider) || String(provider || "Unknown provider");
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
    item.dataset.meetingId = h.meeting_id;
    item.setAttribute("aria-current", String(h.meeting_id === selectedMeetingId));
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
  line.appendChild(text("span", startMs === null ? "…" : offset(startMs), "at"));
  // Only when it changes. Repeating "S0" on ten consecutive lines is noise
  // that makes the actual turn-taking harder to see, not easier.
  if (speaker && speaker !== lastSpeaker) {
    line.appendChild(text("span", speaker, "speaker"));
  }
  line.appendChild(text("span", words, "words"));
  return line;
}

// Page saved history rather than attaching a whole meeting to the layout tree.
// The complete immutable detail still backs copy/export; each page uses the
// same first-speech origin. Stable controls preserve keyboard focus on changes.
function mountSavedTranscript(section, body, segments) {
  const ordered = orderedSegments(segments), origin = ordered[0]?.start_ms ?? 0;
  const pageSize = 200, pages = Math.ceil(ordered.length / pageSize);
  let page = 0, controls, field, previous, next, range;
  const paint = () => {
    clear(body);
    let lastSpeaker = null;
    const start = page * pageSize, end = Math.min(start + pageSize, ordered.length);
    for (let index = start; index < end; index++) {
      const seg = ordered[index];
      body.appendChild(segmentRow(seg.channel, seg.start_ms - origin, seg.speaker, seg.text, lastSpeaker));
      lastSpeaker = seg.speaker || lastSpeaker;
    }
    if (controls) {
      field.value = String(page + 1);
      // Disabling a focused button can clear focus in WebKit. Transfer it
      // before updating boundary state; ordinary page changes keep focus.
      if ((page === 0 && document.activeElement === previous)
          || (page === pages - 1 && document.activeElement === next)) field.focus();
      previous.disabled = page === 0;
      next.disabled = page === pages - 1;
      range.textContent = "Lines " + (start + 1) + "–" + end + " of " + ordered.length
        + " · " + offset(ordered[start].start_ms - origin) + "–" + offset(ordered[end - 1].start_ms - origin);
    }
  };
  const select = requested => {
    if (!Number.isFinite(requested) || !Number.isInteger(requested)) { field.value = String(page + 1); return; }
    const selected = Math.max(0, Math.min(pages - 1, requested));
    if (selected === page) { field.value = String(page + 1); return; }
    page = selected; paint();
    controls.scrollIntoView({block: "nearest"});
  };
  if (pages > 1) {
    controls = text("nav", "", "transcript-pages");
    controls.setAttribute("aria-label", "Saved transcript pages");
    previous = text("button", "Previous"); previous.type = "button";
    previous.setAttribute("aria-label", "Previous transcript page");
    next = text("button", "Next"); next.type = "button";
    next.setAttribute("aria-label", "Next transcript page");
    const label = text("label", "Page ");
    field = document.createElement("input"); field.type = "number";
    field.min = "1"; field.max = String(pages); field.step = "1";
    field.setAttribute("aria-label", "Transcript page");
    label.appendChild(field); label.appendChild(text("span", " of " + pages));
    range = text("p", "", "meta"); range.setAttribute("role", "status");
    field.addEventListener("change", () => select(field.value.trim() === "" ? NaN : Number(field.value) - 1));
    field.addEventListener("keydown", event => {
      if (event.key === "Enter") { event.preventDefault(); select(field.value.trim() === "" ? NaN : Number(field.value) - 1); }
    });
    previous.addEventListener("click", () => select(page - 1));
    next.addEventListener("click", () => select(page + 1));
    for (const child of [previous, label, next, range]) controls.appendChild(child);
    section.appendChild(controls);
  }
  paint();
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
let detailRequest = 0;
let selectedMeetingId = null;

function updateMeetingSelection() {
  if (!el.list) return;
  for (const row of el.list.children) {
    if (row.dataset.meetingId) {
      row.setAttribute("aria-current", String(row.dataset.meetingId === selectedMeetingId));
    }
  }
}

async function openMeeting(id) {
  if (typeof FOTWNotes !== "undefined") FOTWNotes.flushAll();
  selectedMeetingId = id;
  updateMeetingSelection();
  const request = ++detailRequest;
  try {
    const detail = await api("/api/meetings/" + encodeURIComponent(id));
    if (request !== detailRequest) return;
    currentDetail = detail;
    renderDetail(detail);
  } catch (e) {
    if (request === detailRequest) say("Could not open that meeting.");
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
  if (currentDetail && selectedMeetingId === currentDetail.meeting.id && (!meetingId || currentDetail.meeting.id === meetingId)) {
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
let streamSocket = null;
let streamConnecting = false;
let streamRetryTimer = null;
function paintStreamConnection(connected) {
  el.live.hidden = false;
  el.live.textContent = connected ? "Updates connected" : "Updates reconnecting…";
  el.live.classList.toggle("disconnected", !connected);
}
function retryStream() {
  paintStreamConnection(false);
  if (streamRetryTimer !== null) return;
  // One ticket, one connection: reconnecting always mints another ticket.
  streamRetryTimer = setTimeout(() => {
    streamRetryTimer = null;
    return connectStream();
  }, 2000);
}
async function connectStream() {
  if (streamConnecting || streamSocket) return;
  streamConnecting = true;
  if (streamRetryTimer !== null) {
    clearTimeout(streamRetryTimer);
    streamRetryTimer = null;
  }
  try {
    const body = await api("/api/ws-ticket", { method: "POST" });
    const url = "ws://" + window.location.host + "/api/stream?ticket=" + encodeURIComponent(body.ticket);
    const socket = new WebSocket(url);
    streamSocket = socket;
    socket.addEventListener("open", () => {
      if (streamSocket !== socket) return;
      paintStreamConnection(true);
      pollRecording().then(recoverLiveHistory);
    });
    socket.addEventListener("message", (event) => {
      if (streamSocket === socket) handleStreamFrame(JSON.parse(event.data));
    });
    socket.addEventListener("close", () => {
      if (streamSocket !== socket) return;
      streamSocket = null;
      retryStream();
    });
  } catch (e) {
    // A daemon restart can fail the ticket POST as well as the socket. Keep
    // trying either way; never infer recording state from this connection.
    retryStream();
  } finally {
    streamConnecting = false;
  }
}

function handleStreamFrame(frame) {
  if (frame.kind === "resync") {
    recoverLiveHistory();
    refreshLibrary();
  } else if (frame.kind === "meeting_ready") {
    if (frame.reason === "persisted" && frame.session_id && typeof FOTWNotes !== "undefined") {
      FOTWNotes.finishSession(frame.session_id, frame.meeting_id);
    }
    if (frame.reason === "persisted" && frame.session_id === liveSessionId) {
      liveSavedMeetingId = frame.meeting_id;
      adoptSavedLive();
    }
    refreshLibrary(frame.meeting_id);
  } else {
    appendDeltas(frame.deltas || []);
  }
}
function paintLiveNavigation() {
  if (!el.returnLive) return;
  el.returnLive.hidden = !liveSessionId || liveIsShowing();
  el.returnLive.textContent = recState === "recording" ? "Return to recording" : "View last recording";
}
function adoptSavedLive() {
  if (liveSavedMeetingId && recState !== "recording" && liveIsShowing() && selectedMeetingId === null) {
    openMeeting(liveSavedMeetingId);
  }
}

// Section 5.5 budgets ~50 rows in the DOM; a two-hour meeting is ~20k words.
const MAX_ROWS = 200;
// The live pane's finals, kept beside the DOM so that copying them is not
// limited to what the pane still happens to be showing.
//
// Deliberately NOT capped at MAX_ROWS. That constant is a layout budget, and
// an array is not the layout: a four-hour meeting is a few hundred kilobytes
// here, while trimming it would mean a copy that quietly returned the last 200
// lines of a 2,000-line meeting under a status line claiming the transcript.
// Finals only -- a still-revising partial pasted as something somebody said is
// worse than not pasting it.
let liveSegments = [];
// Keep following the live transcript while the reader is at the bottom, but
// leave a deliberate manual scroll alone. The slop accounts for fractional
// layout pixels and makes it easy to resume following by scrolling back down.
const LIVE_SCROLL_SLOP_PX = 48;

function isNearBottom(node) {
  return node.scrollHeight - node.scrollTop - node.clientHeight <= LIVE_SCROLL_SLOP_PX;
}

// Canonical offsets never change. The origin belongs to the full transcript,
// not its visible tail, a speaker, or a selected excerpt.
function orderedSegments(segments) {
  return segments.filter(s => String(s.text || "").trim()).slice().sort((a, b) => a.start_ms - b.start_ms || (a.idx ?? 0) - (b.idx ?? 0));
}
function speechOrigin(segments) {
  return orderedSegments(segments)[0]?.start_ms ?? null;
}
let liveSessionId = null;
let liveFinals = new Map();
let livePartials = new Map();
// Per channel: the end of the latest final or clearing event. A live caption
// starting before it is late text for speech the final already covers (#245).
let liveCovered = new Map();
let liveHistoryMissing = false;
let liveSnapshotOrigin = null;
let liveSavedMeetingId = null;
function selectLiveSession(id) {
  if (!id || id === liveSessionId) return;
  liveSessionId = id;
  liveFinals = new Map();
  livePartials = new Map();
  liveCovered = new Map();
  liveSegments = [];
  liveHistoryMissing = false;
  liveSnapshotOrigin = null;
  liveSavedMeetingId = null;
  recoverLiveHistory();
  if (liveIsShowing()) renderLiveTranscript();
}
async function recoverLiveHistory() {
  const id = liveSessionId;
  if (!id) return;
  try {
    const snapshot = await api("/api/recording/transcript?session_id=" + encodeURIComponent(id));
    if (liveSessionId !== id || snapshot.session_id !== id) return;
    liveHistoryMissing = snapshot.truncated;
    liveSavedMeetingId = snapshot.saved_meeting_id || liveSavedMeetingId;
    if (liveSavedMeetingId && typeof FOTWNotes !== "undefined") FOTWNotes.finishSession(id, liveSavedMeetingId);
    if (typeof snapshot.speech_origin_ms === "number") {
      liveSnapshotOrigin = liveSnapshotOrigin === null ? snapshot.speech_origin_ms
        : Math.min(liveSnapshotOrigin, snapshot.speech_origin_ms);
    }
    // Merge: events may have arrived while this request was in flight.
    appendDeltas(snapshot.deltas);
    adoptSavedLive();
  } catch (_) {
    if (liveSessionId === id) { liveHistoryMissing = true; renderLiveTranscript(); }
  }
}
function liveOrigin() {
  // liveSegments is already sorted when a final changes; partials do not alter it.
  const current = liveSegments[0]?.start_ms ?? null;
  if (liveSnapshotOrigin === null) return current;
  return current === null ? liveSnapshotOrigin : Math.min(current, liveSnapshotOrigin);
}
// One pending row per channel, plus one per live-caption utterance (#245),
// which stays greyed until the final engine's text covers it.
function partialKey(d) {
  return d.utterance ? d.channel + "#" + d.utterance : d.channel;
}
function clearCoveredPartials(channel, endMs) {
  liveCovered.set(channel, Math.max(liveCovered.get(channel) || 0, endMs));
  for (const [key, partial] of livePartials) {
    if (partial.channel === channel && partial.start_ms < endMs) livePartials.delete(key);
  }
}
function appendDeltas(deltas) {
  let finalsChanged = false;
  for (const d of deltas) {
    if (!liveSessionId || d.meeting_id !== liveSessionId) continue;
    if (d.is_final === false) {
      if (d.utterance && d.start_ms < (liveCovered.get(d.channel) || 0)) continue;
      if (d.text) livePartials.set(partialKey(d), d);
      else if (d.utterance) livePartials.delete(partialKey(d));
      else {
        // An empty partial clears the channel's pending text, including live
        // captions it covers (a suppressed echo final sends one).
        livePartials.delete(d.channel);
        clearCoveredPartials(d.channel, d.end_ms);
      }
    } else {
      clearCoveredPartials(d.channel, d.end_ms);
      if (d.text.trim()) {
        liveFinals.set(d.idx, { ...d, speaker: d.channel === "mic" ? "me" : null });
        finalsChanged = true;
      }
    }
  }
  if (finalsChanged) liveSegments = orderedSegments(Array.from(liveFinals.values()));
  renderLiveTranscript();
}
// The cache belongs to the mounted transcript, so navigation/session replacement
// releases it with that DOM. Stable finals keep their nodes across partials.
const liveTranscriptRows = new WeakMap();
function onLiveScroll() {
  const latest = document.getElementById("live-latest");
  if (liveIsShowing() && isNearBottom(el.detail) && latest && !latest.hidden) renderLiveTranscript(true);
}
function renderLiveTranscript(forceFollow = false) {
  if (!liveIsShowing()) return;
  const body = document.getElementById("segments");
  const follow = forceFollow || isNearBottom(el.detail);
  const scrollTop = el.detail.scrollTop;
  const origin = liveOrigin();
  const previous = liveTranscriptRows.get(body) || new Map();
  const next = new Map();
  // Keeping scrollTop alone is insufficient: trimming a row above the reader
  // shifts the words at that position. Freeze the mounted window until the
  // reader resumes following; finals still accumulate for copy and saved replay.
  const paused = !follow && previous.size > 0;
  const visible = [];
  if (paused) {
    for (const key of previous.keys()) {
      const partial = key.startsWith("partial:");
      const d = partial ? livePartials.get(key.slice(8)) : liveFinals.get(Number(key.slice(6)));
      if (d) visible.push({key, d, partial});
    }
  } else {
    visible.push(...liveSegments.slice(-(MAX_ROWS - livePartials.size))
      .map(d => ({key: "final:" + d.idx, d, partial: false})));
    for (const [key, d] of livePartials) visible.push({key: "partial:" + key, d, partial: true});
  }
  for (const {key, d, partial} of visible) {
    const at = origin !== null && d.start_ms >= origin ? d.start_ms - origin : null;
    const speaker = partial && d.channel === "mic" ? "me" : d.speaker;
    const signature = JSON.stringify([d.channel, at, speaker, d.text, partial]);
    let cached = previous.get(key);
    if (!cached || cached.signature !== signature) {
      const row = segmentRow(d.channel, at, speaker, d.text, null);
      if (partial) row.classList.add("pending");
      if (cached) cached.row.replaceWith(row);
      cached = {signature, row};
    }
    next.set(key, cached);
  }
  for (const [key, cached] of previous) if (!next.has(key)) cached.row.remove();
  let index = 0;
  for (const {row} of next.values()) {
    const current = body.children[index++];
    if (current !== row) body.insertBefore(row, current || null);
  }
  liveTranscriptRows.set(body, next);
  const latest = document.getElementById("live-latest");
  if (latest) latest.hidden = !paused;
  const copyRow = document.getElementById("live-copy");
  if (copyRow) copyRow.hidden = liveSegments.length === 0;
  const history = document.getElementById("live-history");
  if (history) history.textContent = liveHistoryMissing
    ? "Some live history is unavailable. The saved transcript may contain more after Stop."
    : "Time since first speech. Times may adjust as earlier speech is finalized.";
  el.detail.scrollTop = follow ? el.detail.scrollHeight : scrollTop;
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
let preparationCancelRequested = false;
let captureStartPending = false;
let recordingAction = 0;
const noteClock = typeof FOTWNotes !== "undefined" ? new FOTWNotes.RecordingClock() : null;
let recordingSince = null;
let autoStopAt = null;
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
  paintRecordingLimit();
}

function paintRecordingLimit() {
  const visible = recState === "recording" && typeof autoStopAt === "number";
  el.recordingLimit.hidden = !visible;
  if (!visible) return;
  const remaining = Math.max(0, autoStopAt - Date.now());
  el.recordingLimit.classList.toggle("ending-soon", remaining <= 5 * 60 * 1000);
  el.recordingLimit.textContent = remaining === 0
    ? "Recording limit reached — waiting for the recorder to stop."
    : "Auto-stop in " + formatElapsed(remaining) + ". If your call has ended, press Stop now.";
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
let recordingRequest = 0;

async function pollRecording() {
  if (!recorderPresent) return;
  const request = ++recordingRequest;
  try {
    const status = await api("/api/recording/status");
    if (request === recordingRequest) renderRecording(status);
  } catch (e) {
    // Not "the request failed" — this build has no recorder at all.
    if (request !== recordingRequest) return;
    if (e.status === 404) { recorderPresent = false; showRecordingControls(false); }
  }
}

function showRecordingControls(visible) {
  el.record.hidden = !visible;
  el.consentLabel.hidden = !visible;
  if (!visible) {
    el.recording.hidden = true;
    el.recordingLimit.hidden = true;
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
  if (captureStopPending) { el.record.disabled = true; return; }
  el.record.textContent = recState === "starting" ? (preparationCancelRequested ? "Stopping…" : "Stop")
    : recState === "preparing" ? (preparationCancelRequested ? "Cancelling…" : "Cancel")
    : recState === "idle" ? (captureStartPending ? "Preparing…" : "Start") : "Stop";
  if (recState === "preparing" || recState === "starting") el.record.disabled = preparationCancelRequested;
  else if (captureStartPending && recState === "idle") el.record.disabled = true;
  else if (recState === "finishing") el.record.disabled = true;
  else if (recState === "recording") el.record.disabled = false;
  else el.record.disabled = !el.consent.checked;
}

function renderRecording(body) {
  noteClock?.update(body);
  if (body.session_id) selectLiveSession(body.session_id);
  const was = recState;
  // Anything the daemon does not name is treated as idle. An unrecognised
  // word must never be the one that leaves a clock running.
  recState =
    ["preparing", "starting", "recording", "finishing"].includes(body.state) ? body.state : "idle";
  preparationCancelRequested = (recState === "preparing" || recState === "starting") && Boolean(body.preparation_cancel_requested);
  renderCaptureHealth(body);
  renderSpeechHealth(body);
  mountLiveNotes();
  paintLiveNavigation();
  adoptSavedLive();
  autoStopAt = body.auto_stop_at_ms ?? null;
  paintRecordingLimit();
  showRecordingControls(true);

  el.recording.hidden = recState === "idle";
  // The badge is static markup ("recording", index.html), so the finishing
  // word has to be written *and* written back, or it sticks for the rest of
  // the tab's life and the next meeting records under a "finishing…" label.
  // ING-11: textContent, as everything in this file writes text.
  el.recording.textContent = recState === "starting" ? "starting capture…" : recState === "preparing" ? "preparing…" : recState === "finishing" ? "finishing…" : "recording";
  // And the class with it (#91). The badge shipped permanently in `.recording`
  // — CON-02's red, which means audio is being captured *right now* — so a
  // meeting whose taps had already closed announced itself in the one colour
  // that must never be diluted. Written back for the same reason the word is:
  // a class left behind outlives the state that set it. See app.css.
  el.recording.className = recState === "preparing" || recState === "starting" ? "preparing" : recState === "finishing" ? "finishing" : "recording";
  if (recState === "preparing" || recState === "starting") lastFinalMs = null;

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
  if (recState === "finishing" || recState === "preparing" || recState === "starting" || captureStartPending) startFinishingPoll();
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
      (recState === "preparing" ? "Preparation: "
        : recState === "starting" ? "Capture setup: "
        : recState === "finishing" ? "Transcription failed during this meeting: "
        : Array.isArray(body.speech_status) && body.speech_status.some((status) => status.epoch !== null && status.epoch !== undefined) ? "Recording notice: "
        : "Recording, but transcription is failing: ") + body.transcription_error,
    );
  }
}

// Actual streams, independent of the preference saved for the next recording.
const speechRows = new Map();
let speechSession = null;
function renderSpeechHealth(body) {
  if (!el.speechHealth) return;
  const sources = Array.isArray(body.speech_status) ? body.speech_status : [];
  if (recState === "idle" || !sources.length) {
    clear(el.speechHealth); speechRows.clear(); speechSession = null;
    el.speechHealth.hidden = true;
    renderLiveStatus();
    return;
  }
  if (speechSession !== (body.session_id || null)) {
    clear(el.speechHealth); speechRows.clear(); speechSession = body.session_id || null;
  }
  const states = {cloud: "Cloud stream selected", local: "Local transcription", connecting: "Connecting", open: "Stream open", reconnecting: "Reconnecting", closed: "Stream closed", complete: "Transcription complete", incomplete: "Transcription incomplete — audio retained"};
  const reasons = {auth: "credentials refused", quota: "quota exhausted", transport_budget: "network retry budget exhausted", rate_limit_budget: "provider retry budget exhausted", speech_stall: "no final during speech", manual: "manual engine change", resume_pending: "resumed saved audio", finishing_tail: "finishing unconfirmed audio locally"};
  const seen = new Set();
  for (const source of sources) {
    if (!["system", "mic"].includes(source.source) || seen.has(source.source)) continue;
    seen.add(source.source);
    let entry = speechRows.get(source.source);
    if (!entry) {
      const row = text("article", "", "speech-source");
      const heading = text("strong", "", "speech-engine");
      heading.id = "speech-source-" + source.source;
      row.setAttribute("aria-labelledby", heading.id);
      const summary = text("p", "", "speech-summary");
      const reason = text("p", "", "speech-explanation");
      const cleanup = text("p", "", "speech-explanation");
      const details = text("details", "", "speech-details");
      details.appendChild(text("summary", "Transcription details"));
      const explanation = text("p", "", "speech-explanation");
      details.appendChild(explanation);
      for (const child of [heading, summary, reason, cleanup, details]) row.appendChild(child);
      entry = {row, heading, summary, reason, cleanup, details, explanation};
      speechRows.set(source.source, entry); el.speechHealth.appendChild(row);
    }
    const label = source.source === "system" ? "System audio" : "Microphone";
    const provider = speechProviderName(source.provider);
    const heading = label + " · " + provider + " / " + String(source.model || "Unknown model");
    if (entry.heading.textContent !== heading) entry.heading.textContent = heading;
    entry.engine = provider + " / " + String(source.model || "Unknown model");
    // Which speech condition needs attention, if any; a new one reopens the bar.
    entry.attention = ["reconnecting", "closed", "incomplete"].includes(source.state) ? source.state
      : source.error ? "error" : source.switch_reason ? "switched:" + source.switch_reason
      : source.captions === "paused" ? "captions_paused" : "";
    entry.stateText = states[source.state] || "Status unavailable";
    const parts = [states[source.state] || "Status unavailable"];
    if (typeof source.pending_audio_ms === "number" && Number.isFinite(source.pending_audio_ms) && source.pending_audio_ms >= 0) {
      parts.push((source.pending_audio_ms / 1000).toFixed(1) + "s audio pending");
    } else parts.push("Coverage not measured");
    if (source.source === "system" && source.diarization_supported === false) parts.push("No remote speaker labels");
    // Live captions (#245) are display-only: pausing them changes nothing saved.
    if (source.captions === "paused") parts.push("Live captions paused");
    const summary = parts.join(" · ");
    if (entry.summary.textContent !== summary) entry.summary.textContent = summary;
    const from = typeof source.from_ms === "number" ? "Coverage from audio " + formatElapsed(source.from_ms) + ": " : "Provider change: ";
    const reason = source.switch_reason ? from + (reasons[source.switch_reason] || "provider changed") : "";
    if (entry.reason.textContent !== reason) entry.reason.textContent = reason;
    entry.reason.hidden = !reason;
    const cleanup = source.worker_running === true && ["complete", "incomplete", "closed"].includes(source.state)
      ? "Finishing transcription work; Start waits for cleanup." : "";
    if (entry.cleanup.textContent !== cleanup) entry.cleanup.textContent = cleanup;
    entry.cleanup.hidden = !cleanup;
    const extra = [];
    if (source.word_timestamps_supported === false) extra.push("Utterance timing only");
    if (source.switch_reason && source.source === "system") extra.push("Speaker identities are separate for each engine");
    if (source.error) extra.push("Notice: " + String(source.error));
    extra.push(source.revision ? "Model revision: " + source.revision : "Model revision not recorded");
    const explanation = extra.join(" · ");
    if (entry.explanation.textContent !== explanation) entry.explanation.textContent = explanation;
  }
  for (const [key, entry] of speechRows) if (!seen.has(key)) { entry.row.remove(); speechRows.delete(key); }
  el.speechHealth.hidden = seen.size === 0;
  renderLiveStatus();
}

// Keep rows mounted between polls so status refreshes preserve keyboard focus.
const captureHealthRows = new Map();
const captureRetryPending = new Map();
let captureHealthSession = null;
let captureStopPending = false;
function renderCaptureHealth(body) {
  if (!el.captureHealth) return;
  const health = body.capture_health;
  const active = recState === "recording" && !captureStopPending && body.session_id && health;
  el.captureHealth.hidden = !active;
  if (!active) {
    clear(el.captureHealth);
    captureHealthRows.clear();
    captureRetryPending.clear();
    captureHealthSession = null;
    renderLiveStatus();
    return;
  }
  if (captureHealthSession !== body.session_id) captureRetryPending.clear();
  captureHealthSession = body.session_id;
  const labels = { system: "System audio", mic: "Microphone" };
  const messages = {
    confirming: "Checking audio delivery",
    healthy: "Healthy",
    quiet: "Silence detected",
    recovering: "Recovering audio",
    exhausted: "Automatic recovery stopped",
    cleanup_blocked: "Audio cleanup incomplete; reopen FlyOnTheWall",
    stopped: "Stopped",
    unavailable: "Unavailable",
  };
  for (const source of ["system", "mic"]) {
    const status = health[source];
    let row = captureHealthRows.get(source);
    if (!row) {
      const container = text("div", "", "capture-source");
      const message = text("span", "");
      message.setAttribute("role", "status");
      const detail = text("span", "", "meta");
      const button = text("button", "Retry " + labels[source].toLowerCase());
      button.type = "button";
      button.addEventListener("click", () => retryCapture(source, button));
      container.appendChild(message);
      container.appendChild(detail);
      container.appendChild(button);
      el.captureHealth.appendChild(container);
      row = { container, message, detail, button };
      captureHealthRows.set(source, row);
    }
    const state = status?.state;
    const pending = captureRetryPending.get(source);
    if (pending && state !== "exhausted") {
      pending.observedTransition = true;
      if (pending.accepted) captureRetryPending.delete(source);
    }
    row.container.setAttribute("data-state", state || "unavailable");
    const message = labels[source] + ": " + (messages[state] || "Status unavailable");
    if (row.message.textContent !== message) row.message.textContent = message;
    const attempts = Number.isInteger(status?.attempts) ? status.attempts : 0;
    const max = Number.isInteger(status?.max_attempts) ? status.max_attempts : 0;
    row.detail.textContent = (attempts || state === "recovering")
      ? attempts + " of " + max + " recovery attempts used" : "";
    if (state === "recovering" && Number.isFinite(status.retry_in_ms)) {
      row.detail.textContent += "; next attempt in " + Math.ceil(Math.max(0, status.retry_in_ms) / 1000) + "s";
    }
    row.button.hidden = state !== "exhausted";
    row.button.disabled = captureRetryPending.has(source);
    row.attention = ["healthy", "confirming"].includes(state) ? "" : (state || "unavailable");
  }
  renderLiveStatus();
}

// The live capture and transcription cards fold into one line so the
// transcript keeps the screen. A source that needs attention (recovering,
// exhausted with its Retry button, quiet, a dropped stream) opens the panel
// when a new problem appears; the user's own collapse is respected while the
// same problems persist.
const LIVE_SOURCE_LABELS = { system: "System audio", mic: "Microphone" };
let liveStatusAttention = new Set();
function renderLiveStatus() {
  const box = el.liveStatus;
  if (!box) return;
  const visible = el.captureHealth?.hidden === false || el.speechHealth?.hidden === false;
  box.hidden = !visible;
  if (!visible) {
    // The next recording starts collapsed, whatever this one needed.
    box.open = false;
    liveStatusAttention = new Set();
    return;
  }
  const parts = [];
  const attention = new Set();
  for (const source of ["system", "mic"]) {
    const capture = el.captureHealth?.hidden === false ? captureHealthRows.get(source) : null;
    const speech = el.speechHealth?.hidden === false ? speechRows.get(source) : null;
    if (!capture && !speech) continue;
    if (capture?.attention) attention.add(source + ":capture:" + capture.attention);
    if (speech?.attention) attention.add(source + ":speech:" + speech.attention);
    const bits = [capture ? capture.message.textContent : LIVE_SOURCE_LABELS[source]];
    if (speech?.engine) bits.push(speech.engine);
    // A healthy capture must not mask a transcription that has stopped.
    if (speech?.attention) bits.push(speech.stateText);
    parts.push(bits.join(" · "));
  }
  const line = parts.join("   |   ") || "Recording status";
  if (el.liveStatusSummary && el.liveStatusSummary.textContent !== line) el.liveStatusSummary.textContent = line;
  box.classList?.toggle("attention", attention.size > 0);
  for (const key of attention) if (!liveStatusAttention.has(key)) { box.open = true; break; }
  liveStatusAttention = attention;
}

async function retryCapture(source, button) {
  const id = captureHealthSession;
  if (!id || recState !== "recording" || captureRetryPending.has(source)) return;
  const request = ++recordingRequest;
  const pending = { accepted: false, observedTransition: false };
  captureRetryPending.set(source, pending);
  const active = () => liveSessionId === id && captureHealthSession === id
    && recState === "recording" && !captureStopPending;
  button.disabled = true;
  try {
    const body = await api("/api/recording/retry?session_id=" + encodeURIComponent(id)
      + "&source=" + encodeURIComponent(source), { method: "POST" });
    if (!active()) return;
    pending.accepted = !body.error;
    if (request === recordingRequest) renderRecording(body);
    if (body.error) say(body.error);
  } catch (e) {
    if (active()) say("Could not request audio recovery. Try again.");
  } finally {
    if (captureRetryPending.get(source) === pending) {
      if (!pending.accepted || pending.observedTransition) captureRetryPending.delete(source);
      button.disabled = captureRetryPending.has(source);
    }
  }
}

// Navigation only: the session model survives showing another meeting.
// Starting a new capture resets the model via status.session_id, never here.
function showLive() {
  if (typeof FOTWNotes !== "undefined") FOTWNotes.flushAll();
  ++detailRequest;
  currentDetail = null;
  selectedMeetingId = null;
  updateMeetingSelection();
  clearDetail();
  el.detail.appendChild(text("h2", "Recording"));
  el.detail.appendChild(
    text("p", "Words appear here as the provider finalizes them.", "meta"),
  );
  if (typeof FOTWNotes !== "undefined") {
    const notes = document.createElement("div");notes.id = "live-notes";el.detail.appendChild(notes);
  }
  const history = text("p", "Time since first speech", "meta");
  history.id = "live-history";
  el.detail.appendChild(history);
  const copyRow = document.createElement("p");
  copyRow.className = "actions";
  // A marker in the DOM rather than a flag beside it, for the same reason
  // `live-pane` is one: `clear(el.detail)` destroys it in the same breath as
  // the pane it belongs to, so no state outlives the thing it describes.
  copyRow.id = "live-copy";
  copyRow.hidden = true;
  copyRow.appendChild(copyButton("Copy transcript", livePayload));
  el.detail.appendChild(copyRow);
  const transcript = document.createElement("section");
  transcript.className = "transcript";
  transcript.id = "live-pane";
  transcript.dataset.sessionId = liveSessionId;
  const body = document.createElement("div");
  body.id = "segments";
  transcript.appendChild(body);
  el.detail.appendChild(transcript);
  const latest = text("button", "Jump to latest", "live-latest");
  latest.id = "live-latest";
  latest.type = "button";
  latest.hidden = true;
  latest.addEventListener("click", () => {
    renderLiveTranscript(true);
    // Focus the newest speech, not the start of a long section. WebKit can
    // scroll focused sections into view even with preventScroll requested.
    const last = body.lastElementChild;
    if (last) { last.tabIndex = -1; last.focus({preventScroll: true}); }
    el.detail.scrollTop = el.detail.scrollHeight;
  });
  el.detail.appendChild(latest);
  // Replace the handler on navigation; no listener accumulates per mount.
  el.detail.onscroll = onLiveScroll;
  renderLiveTranscript();
  mountLiveNotes();
  paintLiveNavigation();
  adoptSavedLive();
}

async function showNoteDrafts() {
  if (typeof FOTWNotes === "undefined") return;
  const request=++detailRequest;currentDetail=null;selectedMeetingId=null;
  updateMeetingSelection();clearDetail();paintLiveNavigation();
  el.detail.appendChild(text("p","Saving pending notes…","meta"));
  await FOTWNotes.flushAll();
  if(request!==detailRequest)return;
  clearDetail();FOTWNotes.mountRecovery(el.detail,api,showRecoveredNote);
}
function showRecoveredNote(id) {
  if (id===liveSessionId && recState==="recording") {showLive();return;}
  if (typeof FOTWNotes === "undefined") return;
  FOTWNotes.flushAll();const request=++detailRequest;currentDetail=null;selectedMeetingId=null;
  updateMeetingSelection();clearDetail();paintLiveNavigation();
  FOTWNotes.mountRecovered(id,el.detail,api,{onSaved:meetingId=>{
    if(request===detailRequest)openMeeting(meetingId);
  }});
}
async function loadNoteDraftControl() {
  if (!el.noteDrafts || typeof FOTWNotes === "undefined") return;
  try {await api("/api/note-drafts?limit=1");el.noteDrafts.hidden=false;}
  catch(e) {el.noteDrafts.hidden=e.status===404;}
}

function mountLiveNotes() {
  if (typeof FOTWNotes === "undefined" || !liveSessionId || !liveIsShowing()) return;
  const parent = document.getElementById("live-notes");
  if (!parent || parent.dataset.sessionId === liveSessionId) return;
  FOTWNotes.flushAll();
  clear(parent);
  const id = liveSessionId;parent.dataset.sessionId = id;
  FOTWNotes.mount(id, parent, api, {session:true, clock:()=>noteClock?.offset(id) ?? null});
}

// Is the live view the thing on screen?
//
// Not answerable from `currentDetail`: it is null while the live pane is up
// and null in a tab that has opened nothing at all, and those are exactly the
// two cases this has to tell apart.
function liveIsShowing() {
  return document.getElementById("live-pane") !== null;
}

async function onRecord() {
  if (recState === "finishing" || captureStopPending || ((recState === "preparing" || recState === "starting") && preparationCancelRequested)) return;
  const stopping = recState === "recording" || recState === "preparing" || recState === "starting";
  const path = stopping ? "/api/recording/stop" : "/api/recording/start?ack=all-party";
  const action = ++recordingAction;
  el.record.disabled = true;
  captureStartPending = !stopping;
  // Poll setup promptly while Start's HTTP request is still waiting on native work.
  if (!stopping) startFinishingPoll();
  if (stopping) {
    captureStopPending = true;
    renderCaptureHealth({});
    noteClock?.stop();
    if (typeof FOTWNotes !== "undefined") FOTWNotes.flushAll();
  }
  paintRecordButton();
  ++recordingRequest;
  try {
    const body = await api(path, { method: "POST" });
    // User actions, unlike status polls, supersede older action replies.
    // A delayed Start answer must never undo Cancel or restore its clock.
    if (action !== recordingAction) return;
    renderRecording(body);
    if (body.error === "consent_required") {
      say("Tick the box first: everyone on the call has to have consented.");
    } else if (body.error) {
      say("The recorder said: " + body.error);
    } else if (body.state === "recording") {
      say("Recording. Tell the other participants.");
      showLive();
    } else if (body.state === "starting") {
      say(body.preparation_cancel_requested ? "Stopping capture. Any buffered audio is being saved." : "Starting capture. Audio may already be buffered; Stop to end it.");
    } else if (body.state === "preparing") {
      say(body.preparation_cancel_requested ? "Cancelled. Waiting for setup to finish releasing its resources." : "Preparing recording. Audio has not started.");
    } else if (body.state === "finishing") {
      say("Stopped. Finishing — writing the meeting to your library.");
    } else {
      say("Stopped. The meeting is being written to your library.");
      await loadMeetings();
    }
  } catch (e) {
    if (action !== recordingAction) return;
    noteClock?.retry();
    pollRecording();
    say("Could not reach the recorder.");
    paintRecordButton();
  } finally {
    if (action === recordingAction) {
      const stopped = captureStopPending;
      captureStopPending = false;
      captureStartPending = false;
      paintRecordButton();
      if (stopped && (recState === "recording" || recState === "preparing" || recState === "starting")) pollRecording();
    }
  }
}

// ------------------------------------------- issue #63, GitHub export

// The same "404 means the feature is absent" convention the recorder uses: a
// read-only build hides the section entirely rather than showing dead knobs.
let githubPresent = true;
let githubSettings = null;

// `GH_ERRORS` and `ghExplain` live in github.js, which the shell loads before
// this file: they are the GitHub module's vocabulary, and the per-meeting sync
// line needs them to explain a failed push (#112).

function renderGithubForm(s) {
  githubSettings = s;
  el.ghRepo.value = s.repo || "";
  el.ghBranch.value = s.branch || "";
  el.ghPrefix.value = s.path_prefix || "";
  el.ghAuto.checked = s.mode === "auto";
  el.ghEnabled.checked = Boolean(s.enabled);
  el.ghPublic.checked = Boolean(s.allow_public_repo);
  el.ghWholeLibrary.checked = Boolean(s.sync_whole_library);
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
        allow_public_repo: el.ghPublic.checked,
        sync_whole_library: el.ghWholeLibrary.checked,
      }),
    });
    if (body.error) {
      // The form keeps what the user typed — wiping it back to the stored
      // values would make them re-enter everything to fix one field.
      say("Not saved. " + body.error.replace("invalid_settings: ", ""));
      el.ghSave.disabled = false;
      return;
    }
    // Accepted: re-render from the reply (the normalized spelling), and redraw
    // the open meeting so its sync line follows the setting that decides it.
    renderGithubForm(body.settings);
    if (currentDetail) renderDetail(currentDetail);
    const auto = body.settings.enabled && body.settings.mode === "auto";
    if (auto && body.settings.sync_whole_library) {
      say(
        "Saved. Every meeting in your library will be synced to " +
          body.settings.repo +
          ", oldest first, about ten a minute.",
      );
    } else if (auto) {
      say("Saved. Meetings will be synced to " + body.settings.repo + " as they finish.");
    } else if (body.settings.enabled) {
      // Not a dead end since #112: a meeting no automatic pass covers offers
      // Sync now, which is the only thing that pushes anything in this mode.
      say(
        "Saved. Nothing is pushed automatically — each finished meeting offers Sync now. " +
          "Tick 'push automatically when a meeting finishes' to have the daemon do it.",
      );
    } else {
      say("Saved. GitHub export is off.");
    }
  } catch (e) {
    say("Could not save the GitHub settings.");
  }
  el.ghSave.disabled = false;
}

// -------------------------------------------------- transcription settings
let speechSaving = false;
function paintSpeechPolicy() {
  const mode = el.speechMode.value;
  el.speechSave.disabled = speechSaving || !["local", "deepgram", "fallback"].includes(mode);
  el.speechLocalFields.hidden = mode === "deepgram" || !mode;
  el.speechDisclosure.textContent = mode === "local"
    ? "Speech recognition stays on this Mac. Summaries and GitHub export have separate settings below and may send text elsewhere."
    : mode === "fallback"
    ? "Audio initially streams to Deepgram (api.deepgram.com). After a bounded provider outage, pending speech is transcribed locally for the rest of the meeting. Start refuses recording if the offline backup cannot prepare. Summaries are configured separately."
    : mode === "deepgram"
    ? "Audio streams to Deepgram (api.deepgram.com) using your configured key. There is no automatic offline backup in this mode. Summaries are configured separately."
    : "Choose a mode explicitly to repair the saved transcription choice.";
}
let captureSaving = false;
let captureSupported = false;
let captureDraftKnown = false;
function paintCapturePolicy() {
  const selected = el.captureSpotify.checked || el.captureBrave.checked;
  el.captureSave.disabled = captureSaving || !captureDraftKnown || (selected && !captureSupported);
}
function onCaptureChange() { captureDraftKnown = true; paintCapturePolicy(); }
function renderCaptureSettings(body) {
  captureSupported = body.supported === true;
  captureDraftKnown = !!body.settings;
  const excluded = body.settings ? [body.settings.ignore_spotify && "Spotify", body.settings.ignore_brave && "Brave"].filter(Boolean) : null;
  el.captureSummary.textContent = excluded ? "Audio exclusions for next recording: " + (excluded.join(", ") || "none") + (excluded.length && !captureSupported ? " (unsupported)" : "") : "Audio exclusions: saved choice unavailable";
  if (body.settings) {
    el.captureSpotify.checked = body.settings.ignore_spotify === true;
    el.captureBrave.checked = body.settings.ignore_brave === true;
  }
  el.captureDisclosure.textContent = "Exclude Spotify or all Brave audio from the system-audio track, including meetings in Brave. Chrome and other meeting apps remain included. Your microphone remains recorded; speaker music can still reach it. Use headphones to avoid that.";
  el.captureStatus.textContent = body.error || (!captureSupported
    ? "Audio exclusions require macOS 26 or newer. If a saved exclusion is enabled, filtered Start is refused. Turn both off explicitly to record the full mix."
    : "Saved for the next recording. The active meeting keeps its existing audio policy.");
  el.captureSettings.hidden = false;
  paintCapturePolicy();
}
async function loadCapture() {
  if (!el.captureSettings || captureSaving) return;
  try { renderCaptureSettings(await api("/api/settings/capture")); }
  catch (e) {
    el.captureSettings.hidden = e.status === 404;
    if (e.status !== 404) {
      el.captureSummary.textContent = "Audio exclusions: saved choice unavailable";
      el.captureStatus.textContent = "Could not load audio exclusions. Retry with Reload saved choice.";
    }
  }
}
async function onCaptureSave() {
  paintCapturePolicy();
  if (el.captureSave.disabled) return;
  captureSaving = true;
  const controls = [el.captureSpotify, el.captureBrave, el.captureReload];
  controls.forEach(control => { control.disabled = true; });
  paintCapturePolicy();
  try {
    const body = await api("/api/settings/capture", {method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({ignore_spotify:el.captureSpotify.checked,ignore_brave:el.captureBrave.checked})});
    if (body.error) el.captureStatus.textContent = "Not saved. " + body.error;
    else renderCaptureSettings(body);
  } catch (_) {
    el.captureSummary.textContent = "Audio exclusions: save unconfirmed — reload saved choice";
    el.captureStatus.textContent = "Save could not be confirmed. Reload saved choice to check; your draft is still here.";
  } finally {
    captureSaving = false;
    controls.forEach(control => { control.disabled = false; });
    paintCapturePolicy();
  }
}

function renderSpeechSettings(body) {
  const settings = body.settings;
  el.speechMode.value = settings?.engine || "";
  el.speechModel.value = settings?.model || "";
  el.speechLanguage.value = settings?.language || "";
  // Live captions (#245): on unless saved off; they need their own model.
  if (el.speechCaptions) el.speechCaptions.checked = settings?.live_captions !== false;
  if (el.speechCaptionsNote) {
    el.speechCaptionsNote.textContent = settings?.engine === "deepgram"
      ? "Live captions run only for on-device meetings; Deepgram shows its own live text."
      : body.captions_installed
        ? "Word-by-word text in the live view, greyed until the final text replaces it. Never saved."
        : "Live captions need their model (parakeet-eou-320ms, NVIDIA Open Model License): install it below or with fotwd stt models install parakeet-eou-320ms.";
  }
  const messages = {
    present_unchecked: "Model file found. Start verifies its contents and prepares native memory before capture in backup mode.",
    missing: "Model not installed. Install it below, or choose an installed model. Start refuses on-device speech until it is installed.",
    unsupported: "Local speech is currently supported only on macOS.",
    not_required: "Deepgram requires a configured key. No local model is used.",
    unknown: "The saved speech choice cannot be read. Select a mode to repair it.",
  };
  const savedMode = {local: "Fully offline speech", deepgram: "Deepgram", fallback: "Deepgram with offline backup"}[settings?.engine];
  el.speechStatus.textContent = body.error || ((savedMode ? "Saved choice: " + savedMode + ". " : "") + (messages[body.model_availability] || "Start checks model readiness."));
  el.speechSettings.hidden = false;
  paintSpeechPolicy();
}
async function loadSpeech() {
  if (!el.speechSettings || speechSaving) return;
  try {
    renderSpeechSettings(await api("/api/settings/speech"));
  } catch (e) {
    el.speechSettings.hidden = e.status === 404;
    if (e.status !== 404) el.speechStatus.textContent = "Could not load transcription settings. Retry with Reload saved choice.";
  }
}
async function onSpeechSave() {
  if (speechSaving || !["local", "deepgram", "fallback"].includes(el.speechMode.value)) return;
  speechSaving = true;
  const controls = [el.speechMode, el.speechModel, el.speechLanguage, el.speechCaptions, el.speechReload].filter(Boolean);
  controls.forEach(control => { control.disabled = true; });
  paintSpeechPolicy();
  el.speechStatus.textContent = "Checking the selected model and saving…";
  try {
    const body = await api("/api/settings/speech", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({engine: el.speechMode.value, model: el.speechModel.value, language: el.speechLanguage.value,
        live_captions: el.speechCaptions ? el.speechCaptions.checked : true}),
    });
    if (body.error) {
      el.speechStatus.textContent = "Not saved. " + body.error;
    } else {
      renderSpeechSettings(body);
      el.speechStatus.textContent = "Saved for the next recording. " + el.speechStatus.textContent;
    }
  } catch (_) {
    el.speechStatus.textContent = "Save could not be confirmed. Reload saved choice to check what is stored; your draft is still here.";
  } finally {
    speechSaving = false;
    controls.forEach(control => { control.disabled = false; });
    paintSpeechPolicy();
  }
}

// ------------------------------------------- issue #74, summarization engine

// Same "404 means the feature is absent" convention as the GitHub section.
let summarizePresent = true;
let summarizeDisclosures = {};

function renderSummarizeForm(body) {
  const s = body.settings;
  const kind = s.policy === "local_only" || s.policy === "anthropic" ? s.policy
    : s.policy === "codex_cli" ? "codex" : s.policy === "claude_cli" ? "claude"
    : !s.policy && body.status.engine === "anthropic" ? "anthropic" : s.cli_kind || "claude";
  el.sumKind.value = kind;
  el.sumBinary.value = s.binary || "";
  el.sumAck.checked = Boolean(s.acknowledged_egress);
  el.sumEnabled.checked = s.policy ? s.policy !== "off"
    : Boolean(s.cli_enabled || body.status.engine === "anthropic");
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
  const enabled = el.sumEnabled.checked, local = el.sumKind.value === "local_only";
  const cli = ["claude", "codex"].includes(el.sumKind.value);
  el.sumAckRow.hidden = !enabled || local;
  el.sumAck.disabled = !enabled || local;
  el.sumBinaryRow.hidden = !cli || !enabled;
  el.sumBinary.disabled = !cli || !enabled;
  if (!enabled || local) el.sumAck.checked = false;
  el.sumDisclosure.textContent = !enabled ? "Summary model calls are off, including stored hosted keys."
    : local ? "On-device summaries require local model setup. No automatic cloud fallback."
    : (summarizeDisclosures[el.sumKind.value] || []).join(" ");
}

function onSummaryDestinationChange() {
  el.sumAck.checked = false;
  paintDisclosure();
}

// What the daemon actually resolves — the diagnostic that cannot lie, since it
// is the daemon's own answer and not the shell's.
function summarizeStatusLine(status) {
  if (status.engine === "anthropic") {
    return "Summary destination: Anthropic API (hosted).";
  }
  if (status.engine === "off") {
    return "Summaries are off. Stored hosted keys are not used.";
  }
  if (status.engine === "local-setup-required") {
    return "Summary destination: On this Mac. Local model setup required; no cloud fallback.";
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
    const policy = !el.sumEnabled.checked ? "off"
      : ({claude:"claude_cli",codex:"codex_cli",anthropic:"anthropic",local_only:"local_only"}[el.sumKind.value] || "off");
    const cli = ["claude_cli", "codex_cli"].includes(policy);
    const hosted = ["anthropic", "claude_cli", "codex_cli"].includes(policy);
    const body = await api("/api/settings/summarize", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        policy,
        cli_enabled: cli,
        acknowledged_egress: hosted && el.sumAck.checked,
        cli_kind: el.sumKind.value === "codex" ? "codex" : "claude",
        binary: cli ? el.sumBinary.value : "",
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
    if (body.status.engine === "local-setup-required") {
      say("Saved. On-device summaries: local model setup required; no cloud fallback.");
    } else if (body.status.engine === "anthropic") {
      say("Saved. Summaries use the hosted Anthropic API.");
    } else if (body.settings.cli_enabled && body.status.binary_resolves) {
      say("Saved. Finished meetings now get a summary and action items.");
    } else if (body.settings.cli_enabled) {
      // Saved but inert: say so rather than promising summaries that will not
      // appear — the exact silence #74 is about.
      say("Saved, but `" + body.status.configured_binary + "` does not resolve here. Enter a full path.");
    } else if (body.settings.policy === "anthropic") {
      say("Saved. Anthropic selected, but an acknowledged stored API key is required.");
    } else {
      say("Saved. Summaries are off.");
    }
  } catch (e) {
    say("Could not save the summarization settings.");
  }
  el.sumSave.disabled = false;
}

// The per-meeting push button used to live here. It is gone (#112): the worker
// owns every push, so a button drawn from the settings alone appeared on
// meetings that were already in the repository and invited a second commit of
// them. `mountGithubSync` in github.js shows each meeting's sync state instead,
// and offers a control only for a meeting whose last attempt failed.

// ----------------------------------------------------- EXP-02, copy to clipboard
//
// EXP-02: "Clipboard writes both text and HTML flavors so a paste lands rich in
// Slack/Notion and plain in an editor." Until now that P0 was satisfied only by
// `MeetingDoc::to_clipboard` over in fotw-store, which nothing outside its own
// tests calls -- the product itself had no way to copy anything, and a user who
// wanted their summary in a message retyped it or dragged a selection across a
// three-column grid.
//
// The payload is built here rather than fetched, for three reasons that point
// the same way:
//
//   * `currentDetail` already holds every byte -- title, summary, segments --
//     so a click needs no request at all.
//   * Safari treats the user gesture as spent at the first suspension point,
//     and a refused write is what a fetch-then-write handler earns there. It
//     works in Chrome, which is exactly the trap §10.1 means when it says all
//     the adversarial testing happens in Safari.
//   * The live pane has no meeting row. Its words are socket deltas that reach
//     the store only after Stop, so no endpoint could serve them -- a
//     server-rendered copy would be a second renderer beside this one rather
//     than a reuse of the Rust one.
//
// Nothing here needs a CSP change: no directive governs the async clipboard
// API, `http://127.0.0.1:<port>` is a secure context (loopback is
// potentially-trustworthy in every engine, and `IngressPolicy` allows no other
// authority), and `require-trusted-types-for 'script'` guards DOM sink
// *assignment* -- not a Blob, not a ClipboardItem, not the serializer below.
// Do not relax the policy for this.
//
// One thing worth saying beside the `no-store` argument in `assets.rs`: the
// clipboard is a wider boundary than this origin. It is system-wide and, with
// Universal Clipboard on, it leaves the machine. That is why every copy here is
// a button a person pressed, and why nothing copies on its own.

const COPY_REFUSED =
  "Could not copy — the browser refused the clipboard. Click this page, then press the button again.";

// `[00:12:34]`, the shape `fotwd export --format txt` writes, rather than the
// pane's `m:ss`. That column is for scanning a transcript beside its audio; a
// line pasted into a document is read on its own, hours later, among other
// people's text.
function hms(ms) {
  const total = Math.max(0, Math.floor((ms || 0) / 1000));
  const pad = (n) => String(n).padStart(2, "0");
  return (
    pad(Math.floor(total / 3600)) +
    ":" +
    pad(Math.floor((total % 3600) / 60)) +
    ":" +
    pad(total % 60)
  );
}

// Who said it, for a copy.
//
// Every copied line carries a name, where the pane prints one only when it
// changes: an unattributed line in somebody else's document is a quote from
// nobody. When diarisation gave us no label the channel is the answer -- that
// is #64's signal, the one §7.5 calls the most valuable structural fact capture
// hands us, and the one the pane makes visible with a tinted band. Deliberately
// not `transcript_lines`' "Speaker" fallback, which has no channel to consult.
function speakerFor(speaker, channel) {
  if (speaker) return speaker;
  return channel === "mic" ? "me" : "them";
}

function lineCount(n) {
  return n + (n === 1 ? " line" : " lines");
}

function plainHead(title, date) {
  return date ? title + "\n" + date + "\n\n" : title + "\n\n";
}

// The detached root every rich flavor is built into: an h1 of the title with
// the date under it, so a paste is a document rather than a fragment of one.
function copyDoc(title, startedAtMs) {
  const root = document.createElement("div");
  root.appendChild(text("h1", title));
  const date = when(startedAtMs);
  if (date) root.appendChild(text("p", date));
  return root;
}

// The rich flavor, as a string.
//
// The serializer rather than reading the markup off the node: `assets.rs` greps
// this file for the markup-assigning property names, and the ordinary read-only
// way to ask an element for its markup is one of them -- so it would fail
// `cargo test --workspace` with a message about XSS that points nowhere near
// serialization. `DOMParser` is out for the opposite reason: it is a real
// Trusted Types sink under this page's policy.
//
// The serializer is the better tool anyway. It escapes by construction, so no
// second escaper joins `fotw_store`'s `escape_html`, and ING-11 follows the
// words onto the clipboard -- `text/html` is *live markup* in whatever
// application receives the paste, and that may be a native app with a parser of
// its own, so a browser's write-time sanitizer is not the defence.
//
// Detached and purpose-built, never the pane itself. Safari sanitizes a written
// `text/html` by re-parsing it with scripting off and keeping only what is
// *visible*, so serializing what is on screen would paste one thing in Safari
// and another in Chrome -- and could carry a hidden row, or these very buttons,
// into somebody's message.
//
// One `xmlns` lands on the outermost element. Every HTML parser ignores an
// attribute it does not know; stripping it would mean string surgery on markup,
// which is the thing this approach exists to avoid.
function serialize(root) {
  return new XMLSerializer().serializeToString(root);
}

// Fills `root` with one paragraph per utterance and returns the matching plain
// text, so both flavors come out of one loop and cannot drift apart.
function transcriptBody(root, segments, origin = speechOrigin(segments)) {
  root.appendChild(text("p", "Time since first speech"));
  let plain = "Time since first speech\n\n";
  for (const seg of orderedSegments(segments)) {
    const head =
      "[" + hms(seg.start_ms - origin) + "] " + speakerFor(seg.speaker, seg.channel) + ":";
    // One line in, one line out. Providers emit newlines inside a segment: in
    // the plain flavor a stray one hides the next timestamp, and in the rich one
    // it collapses to a space anyway. `transcript_lines` flattens for the same
    // reason.
    const words = String(
      seg.text === null || seg.text === undefined ? "" : seg.text,
    ).replace(/[\r\n]+/g, " ");
    // Its own block element per line. One text node with newlines in it pastes
    // into Slack as a single run-on paragraph.
    const line = document.createElement("p");
    // Bold is the timestamp and the name -- strings we built. The words are a
    // sibling text node, so nothing attacker-influenced ever decides what a tag
    // is.
    line.appendChild(text("strong", head));
    line.appendChild(text("span", " " + words));
    root.appendChild(line);
    plain += head + " " + words + "\n";
  }
  return plain;
}

function summaryPayload(detail) {
  const title = detail.meeting.title || "Untitled meeting";
  const root = copyDoc(title, detail.meeting.started_at_ms);
  // The pane's own renderer, one level down from the h1 above. The clipboard and
  // the screen therefore cannot disagree about what a summary looks like: it is
  // the same parse of the same string.
  renderMarkdown(detail.summary_md, root, 1);
  return {
    // The markdown source, verbatim. The plain flavor is the only place markdown
    // can ride -- a `text/markdown` flavor is refused by the browser outright.
    text:
      plainHead(title, when(detail.meeting.started_at_ms)) +
      String(detail.summary_md).trimEnd() +
      "\n",
    html: serialize(root),
    said: "Copied the summary.",
  };
}

function storedTranscriptPayload(detail) {
  const title = detail.meeting.title || "Untitled meeting";
  const root = copyDoc(title, detail.meeting.started_at_ms);
  const plain = transcriptBody(root, detail.segments);
  return {
    text: plainHead(title, when(detail.meeting.started_at_ms)) + plain,
    html: serialize(root),
    said: "Copied the transcript — " + lineCount(detail.segments.length) + ".",
  };
}

// The live pane's own words, from `liveSegments` rather than from the rendered
// rows: the pane is trimmed to MAX_ROWS and holds a still-revising partial, so a
// copy taken off the screen would drop the top of a long meeting and paste half
// a sentence at the bottom. No title -- there is no meeting row yet, and a
// heading invented for one would be the only line in the paste nobody said.
// "so far" is what says the meeting is still running.
function livePayload() {
  const root = document.createElement("div");
  if (liveHistoryMissing) root.appendChild(text("p", "Some live history is unavailable; this copy may be incomplete."));
  const plain = transcriptBody(root, liveSegments, liveOrigin());
  return {
    text: (liveHistoryMissing ? "Some live history is unavailable; this copy may be incomplete.\n\n" : "") + plain,
    html: serialize(root),
    said: "Copied the transcript so far — " + lineCount(liveSegments.length) + ".",
  };
}

// The one place anything touches the clipboard.
//
// Not `async`, and it reaches the write with nothing suspended before it: Safari
// treats the user gesture as spent at the first suspension point and Firefox
// requires transient activation. Everything needed is already in memory, so the
// handler is straight-line.
//
// There is deliberately no single-flavor retry in the rejection handler. It
// would run in a promise reaction job, after the gesture is gone, so in Safari
// it would fail too -- and it would drop the HTML flavor EXP-02 exists for. One
// write, one sentence.
//
// Feature detection rather than a try/catch: off a secure context the whole
// `navigator.clipboard` object is absent rather than a rejecting stub, so the
// call is a TypeError that no `.catch` on the promise would ever see. This
// daemon cannot serve such an origin -- `IngressPolicy::for_loopback_port`
// allows the single authority `127.0.0.1:<port>` and `check_host` compares the
// whole string, so a request by hostname or LAN address is refused before this
// file loads. The guard is belt and braces against a future bind, not a
// supported path, which is why there is no fallback behind it.
function copyNow(payload) {
  if (!(navigator.clipboard && navigator.clipboard.write && window.ClipboardItem)) {
    say(COPY_REFUSED);
    return;
  }
  // ONE item with two keys, never two items: the macOS pasteboard keeps only the
  // first, so a second would drop the rich flavor with no error anywhere. Blobs
  // rather than bare strings, each typed to match the key it is filed under -- a
  // string value only became universal in Chrome 133, and a Blob whose type
  // disagrees with its key is refused outright.
  const item = new ClipboardItem({
    "text/plain": new Blob([payload.text], { type: "text/plain" }),
    "text/html": new Blob([payload.html], { type: "text/html" }),
  });
  navigator.clipboard.write([item]).then(
    () => say(payload.said),
    // Chrome grants this to a focused page and refuses an unfocused one
    // ("Document is not focused"), which is what clicking into devtools
    // mid-copy looks like. Said out loud: a copy that quietly did not happen is
    // discovered at the paste, in front of other people.
    () => say(COPY_REFUSED),
  );
}

function copyButton(label, build) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "copy";
  // The accessible name never changes -- no "Copied!" flip -- and the button is
  // never disabled. `#detail` is itself aria-live="polite" and `renderDetail`
  // rewrites all of it, so a label that changed in place would be announced as
  // part of a region that re-announces on every meeting somebody opens; and
  // disabling a focused button takes focus off the person who just pressed it.
  // The outcome is said once, through `say()`, whose role="status" line lives
  // outside the pane.
  button.textContent = label;
  button.addEventListener("click", () => copyNow(build()));
  return button;
}

// The per-meeting controls, in one row: the copies.
//
// Each control is drawn only when it can do something -- no summary, no "Copy
// summary"; no segments, no "Copy transcript" -- and the row is not appended
// when it would be empty. That was `githubActions`' old rule, and it is the
// rule #112 applied to the export as well: the push button that was drawn here
// whenever export was enabled is gone, because "enabled" was never the same
// question as "is there anything to do for this meeting".
function actionsRow(detail) {
  const row = document.createElement("p");
  row.className = "actions";
  if (detail.summary_md) {
    row.appendChild(copyButton("Copy summary", () => summaryPayload(detail)));
  }
  if (detail.segments.length) {
    row.appendChild(
      copyButton("Copy transcript", () => storedTranscriptPayload(detail)),
    );
  }
  return row.firstChild ? row : null;
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
  el.returnLive.addEventListener("click", showLive);
  el.noteDrafts?.addEventListener("click", showNoteDrafts);
  // Re-evaluates the disabled state; the box gates Start and nothing else.
  // Through `paintRecordButton` so it is state-aware: ticking the box while a
  // meeting is being written must not re-enable a button with nothing to do.
  el.consent.addEventListener("change", paintRecordButton);
  el.ghSave.addEventListener("click", onGithubSave);
  // An acknowledgement belongs to the repository it was given beside. Editing
  // the name clears it, so a tick given for one public repository is not
  // carried over to the next name typed.
  el.ghRepo.addEventListener("input", function () {
    if (!githubSettings || el.ghRepo.value.trim() !== githubSettings.repo) {
      el.ghPublic.checked = false;
    }
  });
  el.speechSave?.addEventListener("click", onSpeechSave);
  el.captureSpotify?.addEventListener("change", onCaptureChange);
  el.captureBrave?.addEventListener("change", onCaptureChange);
  el.captureSave?.addEventListener("click", onCaptureSave);
  el.captureReload?.addEventListener("click", loadCapture);
  el.speechReload?.addEventListener("click", loadSpeech);
  el.speechMode?.addEventListener("change", paintSpeechPolicy);
  el.sumSave.addEventListener("click", onSummarizeSave);
  // The disclosure differs per engine, so it follows the picker rather than
  // waiting for a save.
  el.sumKind.addEventListener("change", onSummaryDestinationChange);
  el.sumEnabled.addEventListener("change", paintDisclosure);
  el.ghSettings.addEventListener("toggle", function () {
    if (el.ghSettings.open) loadGithubRepos();
  });
  await loadCapture();
  await loadSpeech();
  const modelSetup = document.getElementById("model-setup-mount");
  if (modelSetup && typeof FOTWModels !== "undefined") FOTWModels.mount(modelSetup, api);
  await loadMeetings();
  await loadNoteDraftControl();
  await loadGithub();
  await loadSummarize();
  openRequestedSettings();
  await pollRecording();
  setInterval(pollRecording, RECORDING_POLL_MS);
  connectStream();
}

main();

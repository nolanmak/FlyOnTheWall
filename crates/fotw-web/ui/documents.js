// Sharing drafts stay in memory across meeting navigation until saved. Nothing
// here sends a message. The configured GitHub worker can sync saved briefs.
const sharingEditors = new Map();

function sharingTime(ms) {
  return new Intl.DateTimeFormat("en-US", {
    timeZone: "America/New_York", year: "numeric", month: "short", day: "numeric",
    hour: "numeric", minute: "2-digit", second: "2-digit", timeZoneName: "short",
  }).format(new Date(ms));
}
function sharingOffset(ms) {
  const seconds = Math.floor(ms / 1000);
  return [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60]
    .map(n => String(n).padStart(2, "0")).join(":");
}
function sharingEscaped(value) {
  return String(value).replace(/[\\`*_{}\[\]<>#!|]/g, "\\$&");
}
function sharingSelection(editor) {
  return editor.document.excerpts.filter(e => !editor.excluded.has(e.index));
}
function sharingCorrectedSource(doc, value) {
  const rules = doc.name_corrections || [];
  if (!rules.length) return value;
  const byName = new Map(rules.filter(r => r.from).map(r => [r.from, r.to]));
  const escaped = Array.from(byName.keys()).sort((a,b) => b.length-a.length)
    .map(name => name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  if (!escaped.length) return value;
  const pattern = new RegExp("(?<![\\p{L}\\p{N}])(?:" + escaped.join("|") + ")(?![\\p{L}\\p{N}])", "gu");
  return value.replace(pattern, name => "[" + byName.get(name) + "]");
}
function sharingMarkdown(editor) {
  const doc = editor.document;
  let md = editor.markdown.trim() + "\n\nMeeting: " + sharingTime(doc.started_at_ms) + "\n";
  if (!editor.includeTranscript) return md;
  if (doc.name_corrections?.length) md += "\nUser-confirmed name corrections appear in brackets. The original transcript is unchanged.\n";
  md += "\n## Selected meeting transcript\n\nSharing edition; omissions are marked. Source wording is preserved. Speaker labels may be incomplete.\n";
  let next = 0;
  for (const e of sharingSelection(editor)) {
    if (e.index > next) md += "\n[Conversation omitted from sharing copy.]\n";
    md += "\n> [" + sharingOffset(e.offset_ms) + " | " + sharingTime(e.at_ms) + "] "
      + sharingEscaped(e.speaker) + ": " + sharingEscaped(sharingCorrectedSource(doc, e.text)).replace(/\n/g, "\n> ") + "\n";
    next = e.index + 1;
  }
  if (next < doc.source_segments) md += "\n[Remaining conversation omitted from sharing copy.]\n";
  return md;
}
function sharingFilename(doc) {
  return (doc.title.replace(/[^\p{L}\p{N} _-]/gu, "").trim().slice(0, 100) || "Meeting document");
}
function downloadSharingDocument(editor) {
  const filename = sharingFilename(editor.document) + ".md";
  const url = URL.createObjectURL(new Blob([sharingMarkdown(editor)], {type: "text/markdown;charset=utf-8"}));
  const a = document.createElement("a");
  a.href = url; a.download = filename;
  try {
    document.body.appendChild(a); a.click();
  } finally {
    a.remove(); setTimeout(() => URL.revokeObjectURL(url), 30000);
  }
  return filename;
}
function sharingPreview(editor, root) {
  clear(root);
  renderMarkdown(editor.markdown, root, 0);
  root.appendChild(text("p", "Meeting: " + sharingTime(editor.document.started_at_ms), "document-date"));
  if (!editor.includeTranscript) return;
  if (editor.document.name_corrections?.length) root.appendChild(text("p", "User-confirmed name corrections appear in brackets. The original transcript is unchanged.", "meta"));
  const section = document.createElement("section");
  section.className = "document-transcript";
  section.appendChild(text("h2", "Selected meeting transcript"));
  section.appendChild(text("p", "Sharing edition; omissions are marked. Source wording is preserved. Speaker labels may be incomplete.", "document-date"));
  let next = 0;
  for (const e of sharingSelection(editor)) {
    if (e.index > next) section.appendChild(text("p", "[Conversation omitted from sharing copy.]", "document-omission"));
    const row = document.createElement("div");
    row.className = "document-excerpt";
    row.appendChild(text("p", sharingOffset(e.offset_ms) + " | " + sharingTime(e.at_ms) + " · " + e.speaker, "document-stamp"));
    row.appendChild(text("p", sharingCorrectedSource(editor.document, e.text)));
    section.appendChild(row);
    next = e.index + 1;
  }
  if (next < editor.document.source_segments) section.appendChild(text("p", "[Remaining conversation omitted from sharing copy.]", "document-omission"));
  root.appendChild(section);
}
function printSharingDocument(editor) {
  let root = document.getElementById("sharing-print");
  if (root) root.remove();
  root = document.createElement("article");
  root.id = "sharing-print";
  sharingPreview(editor, root);
  document.body.appendChild(root);
  const oldTitle = document.title;
  document.title = sharingFilename(editor.document);
  const cleanup = () => { root.remove(); document.title = oldTitle; };
  window.addEventListener("afterprint", cleanup, { once: true });
  window.print();
}
async function sharingRequest(id, body) {
  const result = await api("/api/meetings/" + encodeURIComponent(id) + "/document", {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
  });
  if (result.error) throw new Error(result.error);
  return result;
}
function mountSharingDocument(detail, host) {
  if (detail.meeting.state !== "ready") return;
  const id = detail.meeting.id;
  let editor = sharingEditors.get(id);
  if (!editor) {
    editor = { document: null, markdown: "", revision: 0, purpose: "", audience: "",
      busy: false, loaded: false, dirty: false, automatic: true, includeTranscript: true,
      excluded: new Set(), message: "Loading document…" };
    sharingEditors.set(id, editor);
  }
  const panel = document.createElement("section");
  panel.className = "sharing-panel";
  host.appendChild(panel);
  editor.render = render;

  function accept(state) {
    editor.document = state.document; editor.revision = state.revision;
    editor.automatic = state.automatic; editor.loaded = true;
    editor.markdown = state.document ? state.document.markdown : "";
    editor.excluded = new Set(); editor.dirty = false;
  }
  async function run(body, label, download = false) {
    if (editor.busy) return;
    editor.busy = true; editor.downloading = download; editor.message = label; editor.render();
    try {
      const state = await sharingRequest(id, body);
      if (body.action === "configure") editor.automatic = state.automatic;
      else accept(state);
      editor.message = body.action === "configure" ? "Preference saved." : "";
      if (download) startDownload();
      if (body.action === "correct_name" && typeof currentDetail !== "undefined"
          && currentDetail?.meeting.id === id && typeof openMeeting === "function") {
        await openMeeting(id);
      }
    } catch (e) { editor.message = e.message === "request failed" ? "Could not load or save the document. Check the connection and retry." : e.message; }
    editor.busy = false; editor.downloading = false; editor.render();
  }
  function button(label, handler, disabled = false) {
    const b = text("button", label); b.type = "button"; b.disabled = disabled;
    b.addEventListener("click", handler); return b;
  }
  function field(label, key, placeholder, max) {
    const l = text("label", label, "document-field");
    const input = document.createElement("input"); input.value = editor[key] || ""; input.placeholder = placeholder;
    input.maxLength = max; input.disabled = editor.busy;
    input.addEventListener("input", () => { editor[key] = input.value; }); l.appendChild(input); return l;
  }
  function checkbox(label, checked, handler) {
    const l = document.createElement("label"); l.className = "document-check";
    const c = document.createElement("input"); c.type = "checkbox"; c.checked = checked; c.disabled = editor.busy;
    c.addEventListener("change", () => handler(c.checked)); l.append(c, document.createTextNode(label)); return l;
  }
  function startDownload() {
    try {
      const filename = downloadSharingDocument(editor);
      editor.message = "Download started: " + filename;
    } catch (_) {
      editor.message = "Your draft is saved, but the download could not start. Click Download .md to retry.";
    }
  }
  function render() {
    if (!panel.isConnected) return;
    clear(panel);
    panel.appendChild(text("h3", "Meeting document"));
    panel.appendChild(text("p", "Create a document from this meeting and download it automatically. Your browser saves the file to its download folder.", "meta"));
    if (typeof githubSettings !== "undefined" && githubSettings?.enabled && githubSettings.mode === "auto") {
      panel.appendChild(text("p", "Saved briefs and summaries sync automatically to " + githubSettings.repo + ".", "meta"));
    }
    const actions = document.createElement("div"); actions.className = "document-actions";
    const create = () => run({action: "generate", purpose: editor.purpose, audience: editor.audience,
      revision: editor.revision}, "Creating your document… It will download when ready.", true);
    const primary = button("", () => {
      if (!editor.document) return create();
      if (editor.dirty) return run({action: "save", revision: editor.revision, markdown: editor.markdown,
        excluded_indices: Array.from(editor.excluded)}, "Saving your changes…", true);
      startDownload(); editor.render();
    });
    // The spinner is CSS on the button itself, so the label stays the button's whole text.
    primary.className = editor.busy && editor.downloading ? "document-primary is-busy" : "document-primary";
    const updatePrimary = () => {
      primary.textContent = !editor.document ? "Create & download .md"
        : editor.dirty ? "Save & download .md" : "Download .md";
      primary.disabled = editor.busy || !editor.loaded || (editor.dirty && !editor.markdown.trim());
    };
    updatePrimary(); actions.appendChild(primary);
    if (editor.document) actions.appendChild(button("Print / PDF", () => printSharingDocument(editor), editor.busy));
    if (!editor.loaded && !editor.busy) actions.appendChild(button("Retry", () => run({action: "load"}, "Loading document…"), editor.busy));
    panel.appendChild(actions);
    const status = document.createElement("p"); status.className = editor.busy ? "document-status is-busy" : "document-status";
    status.setAttribute("role", "status");
    if (editor.busy) {
      const spinner = document.createElement("span"); spinner.className = "document-spinner";
      spinner.setAttribute("aria-hidden", "true"); status.appendChild(spinner);
    }
    status.appendChild(document.createTextNode(editor.message)); panel.appendChild(status);

    const options = document.createElement("details"); options.className = "document-options";
    options.open = Boolean(editor.optionsOpen);
    options.addEventListener("toggle", () => { editor.optionsOpen = options.open; });
    options.appendChild(text("summary", "Customize"));
    const preferences = document.createElement("div"); preferences.className = "document-preferences";
    preferences.append(field("Meeting purpose", "purpose", "Figure it out from the meeting", 2000),
      field("Who is it for?", "audience", "E.g. videographer or client", 500));
    options.appendChild(preferences);
    options.appendChild(checkbox("Include the selected transcript", editor.includeTranscript, value => {
      editor.includeTranscript = value;
      if (editor.document) sharingPreview(editor, preview);
    }));
    options.appendChild(checkbox("Prepare drafts automatically after meetings", editor.automatic,
      automatic => run({action: "configure", automatic}, "Saving preference…")));
    options.appendChild(text("p", "Background drafts stay in the app. Downloads start when you click Create or Download.", "meta"));
    const regenerate = editor.document ? button("Create new draft & download .md", create, editor.busy || editor.dirty) : null;
    const moreActions = document.createElement("div"); moreActions.className = "document-actions";
    if (regenerate) moreActions.appendChild(regenerate);
    moreActions.appendChild(button("Reload saved draft", () => run({action: "load"}, "Loading document…"), editor.busy || editor.dirty));
    options.appendChild(moreActions);
    panel.appendChild(options);
    if (!editor.document) return;

    const review = document.createElement("details"); review.className = "document-edit";
    review.open = Boolean(editor.reviewOpen);
    review.addEventListener("toggle", () => { editor.reviewOpen = review.open; });
    review.appendChild(text("summary", "Review or edit"));
    const names = document.createElement("div"); names.className = "document-preferences";
    names.append(field("Name as transcribed", "incorrectName", "Incorrect spelling", 100),
      field("Correct name", "correctName", "Verified spelling", 100));
    const correctName = button("Remember name correction", () => run({action: "correct_name",
      revision: editor.revision, incorrect_name: editor.incorrectName || "", correct_name: editor.correctName || ""},
      "Saving name correction…"), editor.busy || editor.dirty);
    review.append(names, correctName);
    review.appendChild(text("p", "Applies to this meeting’s summary, document, and future regeneration. Original audio and transcript stay intact.", "meta"));
    for (const rule of editor.document.name_corrections || []) {
      review.appendChild(text("p", rule.from + " → " + rule.to, "meta"));
    }
    if (editor.document.review_notes.length) {
      const notes = document.createElement("div"); notes.className = "document-review";
      notes.appendChild(text("strong", "Details to check before sharing"));
      const list = document.createElement("ul");
      for (const note of editor.document.review_notes) list.appendChild(text("li", note));
      notes.appendChild(list); review.appendChild(notes);
    }
    const area = document.createElement("textarea"); area.value = editor.markdown;
    area.maxLength = 100000; area.disabled = editor.busy; area.setAttribute("aria-label", "Document text (Markdown)");
    const dirtyLabel = text("p", editor.dirty ? "Unsaved changes. Save & download when you’re ready." : "", "meta");
    const markDirty = () => {
      editor.dirty = true; dirtyLabel.textContent = "Unsaved changes. Save & download when you’re ready.";
      updatePrimary(); if (regenerate) regenerate.disabled = true;
      correctName.disabled = true;
      sharingPreview(editor, preview);
    };
    area.addEventListener("input", () => { editor.markdown = area.value; markDirty(); });
    review.append(area, dirtyLabel);
    const selection = document.createElement("details"); selection.className = "document-selection";
    const selectionTitle = text("summary", "Selected transcript · " + sharingSelection(editor).length + " excerpts");
    selection.appendChild(selectionTitle);
    selection.appendChild(text("p", "Uncheck anything you don’t want in the file. The original meeting stays intact.", "meta"));
    for (const e of editor.document.excerpts) {
      selection.appendChild(checkbox(sharingOffset(e.offset_ms) + " · " + e.text, !editor.excluded.has(e.index), keep => {
        if (keep) editor.excluded.delete(e.index); else editor.excluded.add(e.index);
        selectionTitle.textContent = "Selected transcript · " + sharingSelection(editor).length + " excerpts";
        markDirty();
      }));
    }
    review.appendChild(selection); panel.appendChild(review);
    panel.appendChild(text("p", "For: " + editor.document.audience, "meta"));
    const preview = document.createElement("article"); preview.className = "sharing-preview";
    sharingPreview(editor, preview); panel.appendChild(preview);
  }
  render();
  if (!editor.dirty && !editor.busy) run({action: "load"}, "Loading document…");
  // A meeting-ready event may have replaced this panel while generation ran.
  // The closure uses editor.render, so results land in the current panel.
}

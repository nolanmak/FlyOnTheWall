// The post-meeting cleanup (#246): progress in the meeting view, the switch
// between the cleaned and the live transcript, and when cleanup runs.
// Metadata only; the transcript itself is reloaded through the meeting API.
globalThis.FOTWCleanup = {
  mount(id, parent, api, reload) {
    const make = (tag, value) => { const n = document.createElement(tag); n.textContent = value; return n; };
    const root = document.createElement('section'); root.className = 'cleanup'; root.hidden = true;
    const message = root.appendChild(make('p', ''));
    message.setAttribute('role', 'status'); message.setAttribute('aria-live', 'polite');
    const note = root.appendChild(make('p', '')); note.className = 'meta'; note.hidden = true;
    const toggle = root.appendChild(make('button', 'Show live transcript'));
    toggle.type = 'button'; toggle.hidden = true;
    parent.appendChild(root);
    const base = '/api/meetings/' + encodeURIComponent(id) + '/cleanup';
    let request = 0, timer = false, wasPending = false, showing = null;
    const current = seq => root.isConnected !== false && seq === request;
    function poll() {
      if (timer || root.isConnected === false) return;
      timer = true;
      setTimeout(async () => { timer = false; if (root.isConnected !== false) await load(); }, 2000);
    }
    function render(state) {
      showing = state.showing;
      root.hidden = state.phase === 'none';
      toggle.hidden = state.phase !== 'done' || !state.showing;
      toggle.textContent = state.showing === 'live' ? 'Show cleaned transcript' : 'Show live transcript';
      note.hidden = !state.detail;
      note.textContent = state.detail || '';
      if (state.phase === 'pending') {
        message.textContent = 'Cleaning up transcript… ' + Math.max(0, Math.min(100, state.percent | 0)) + '%';
        wasPending = true;
        poll();
      } else if (state.phase === 'done') {
        message.textContent = state.showing === 'live'
          ? 'Showing the live transcript. A cleaned version is available.'
          : 'Cleaned transcript: re-transcribed on this Mac after the meeting.';
        // The cleaned version became primary while this view was open.
        if (wasPending) { wasPending = false; reload(); }
      } else if (state.phase === 'failed') {
        message.textContent = 'Transcript cleanup did not finish. Showing the live transcript.';
        wasPending = false;
      }
    }
    async function load(version) {
      if (root.isConnected === false) return;
      const seq = ++request;
      try {
        const state = version
          ? await api(base + '/show', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ version }) })
          : await api(base);
        if (!current(seq)) return;
        render(state);
        if (version) { reload(); }
      } catch (error) {
        if (current(seq)) root.hidden = true;
      }
    }
    toggle.addEventListener('click', () => load(showing === 'live' ? 'cleaned' : 'live'));
    load();
    return root;
  },

  settings(parent, api) {
    const make = (tag, value) => { const n = document.createElement(tag); n.textContent = value; return n; };
    const root = document.createElement('div'); root.className = 'cleanup-settings'; root.hidden = true;
    const label = root.appendChild(make('label', 'Clean up the transcript after a meeting '));
    label.className = 'gh-field';
    const select = label.appendChild(document.createElement('select'));
    select.id = 'cleanup-mode';
    for (const [value, text] of [
      ['auto', 'Automatic: offline meetings only'],
      ['on', 'Always, Deepgram meetings too'],
      ['off', 'Off'],
    ]) { const o = select.appendChild(make('option', text)); o.value = value; }
    const explain = root.appendChild(make('p', 'After you stop, the saved audio is re-transcribed on this Mac with Parakeet and the other participants get speaker labels. The live transcript is kept; you can switch back to it on the meeting. Summaries wait up to 15 minutes for the cleaned version.'));
    explain.className = 'gh-note';
    const status = root.appendChild(make('p', '')); status.className = 'gh-note'; status.setAttribute('role', 'status');
    parent.appendChild(root);
    let request = 0;
    async function save() {
      const seq = ++request;
      select.disabled = true;
      try {
        const doc = await api('/api/settings/cleanup', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ mode: select.value }) });
        if (seq !== request) return;
        select.value = doc.mode;
        status.textContent = 'Saved. Applies to meetings you stop from now on.';
      } catch (error) {
        if (seq === request) status.textContent = 'Could not save the cleanup setting.';
      } finally {
        if (seq === request) select.disabled = false;
      }
    }
    select.addEventListener('change', save);
    (async () => {
      try {
        const doc = await api('/api/settings/cleanup');
        select.value = doc.mode;
        root.hidden = false;
      } catch (error) {
        root.hidden = true;
      }
    })();
    return root;
  },
};

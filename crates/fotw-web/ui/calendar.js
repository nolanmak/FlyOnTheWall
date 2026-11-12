// Calendar settings: Apple Calendar status, and an optional Google Calendar
// connection through Composio. The Composio key goes to the daemon once and
// is never shown again; the page only learns whether one is saved.
globalThis.FOTWCalendar = {
  settings(parent, api) {
    const ROUTE = '/api/settings/calendar';
    const POLL_MS = 3000, POLL_LIMIT = 100;
    const make = (tag, value, id) => { const n = document.createElement(tag); if (value) n.textContent = value; if (id) n.id = id; return n; };
    const note = (value, id) => { const n = make('p', value, id); n.className = 'gh-note'; return n; };

    const root = make('details', '', 'calendar-settings'); root.className = 'gh-settings'; root.hidden = true;
    root.appendChild(make('summary', 'Calendars'));
    root.appendChild(note('Recordings are named after the calendar event they overlap.'));
    const apple = root.appendChild(note('', 'calendar-apple'));
    root.appendChild(note('Apple Calendar includes any Google accounts added in System Settings › Internet Accounts.'));

    root.appendChild(make('h3', 'Google Calendar via Composio'));
    root.appendChild(note('Event titles and times pass through Composio (backend.composio.dev) when a recording is saved. Audio, transcripts and summaries never do.'));
    const keyState = root.appendChild(note('', 'calendar-key-state'));
    const keyRow = root.appendChild(make('div', '', 'calendar-key-row'));
    const keyLabel = keyRow.appendChild(make('label', 'Composio API key ')); keyLabel.className = 'gh-field';
    const key = keyLabel.appendChild(make('input', '', 'calendar-key'));
    key.type = 'password'; key.autocomplete = 'off'; key.spellcheck = false; key.maxLength = 512;
    const keySave = keyRow.appendChild(make('button', 'Save key', 'calendar-key-save')); keySave.type = 'button';
    const keyForget = root.appendChild(make('button', 'Forget key', 'calendar-key-forget')); keyForget.type = 'button';

    const ackLabel = root.appendChild(make('label', '')); ackLabel.className = 'gh-field';
    const ack = ackLabel.appendChild(make('input', '', 'calendar-ack')); ack.type = 'checkbox';
    ackLabel.appendChild(make('span', ' I understand event titles and times pass through Composio.'));
    const connect = root.appendChild(make('button', 'Connect Google Calendar', 'calendar-connect')); connect.type = 'button';
    const signin = root.appendChild(make('a', 'Continue to Google sign-in', 'calendar-signin'));
    signin.target = '_blank'; signin.rel = 'noopener noreferrer'; signin.hidden = true;
    const accounts = root.appendChild(make('ul', '', 'calendar-accounts'));
    const message = root.appendChild(note('', 'calendar-status')); message.setAttribute('role', 'status');
    const retry = root.appendChild(make('button', 'Try again', 'calendar-retry')); retry.type = 'button'; retry.hidden = true;
    parent.appendChild(root);

    let current = null, busy = false, polls = 0, timer = null;
    const words = { active: 'Connected', pending: 'Waiting for Google sign-in…', failed: 'Sign-in failed or expired. Disconnect and connect again.' };

    function paint(doc) {
      current = doc;
      root.hidden = false;
      apple.textContent = 'Apple Calendar: ' + (doc.apple_access || 'unknown');
      keyRow.hidden = Boolean(doc.composio_key_saved);
      keyForget.hidden = !doc.composio_key_saved;
      keyState.textContent = doc.composio_key_saved ? 'Composio key saved in your keychain.' : 'Paste a Composio API key to connect Google accounts.';
      if (doc.composio_acknowledged) ack.checked = true;
      ackLabel.hidden = Boolean(doc.composio_acknowledged);
      connect.textContent = (doc.google_accounts || []).length ? 'Connect another Google account' : 'Connect Google Calendar';
      const list = (doc.google_accounts || []).map((account, i) => {
        const item = make('li', 'Google account ' + (i + 1) + ': ' + (words[account.status] || account.status) + ' ');
        const drop = item.appendChild(make('button', 'Disconnect')); drop.type = 'button';
        drop.addEventListener('click', () => act({ action: 'disconnect', id: account.id }));
        return item;
      });
      accounts.replaceChildren(...list);
      const url = typeof doc.redirect_url === 'string' && doc.redirect_url.startsWith('https://') ? doc.redirect_url : '';
      if (url) { signin.href = url; signin.hidden = false; }
      if (!(doc.google_accounts || []).some(a => a.status === 'pending')) signin.hidden = true;
      message.textContent = doc.error || (url ? 'Sign in on the Google page, then come back here.' : '');
      enable();
      schedule();
    }

    function enable() {
      const saved = Boolean(current && current.composio_key_saved);
      connect.disabled = busy || !saved || !(ack.checked || (current && current.composio_acknowledged));
      keySave.disabled = busy;
      keyForget.disabled = busy;
    }

    function schedule() {
      const pending = current && (current.google_accounts || []).some(a => a.status === 'pending');
      if (!pending || timer !== null || polls >= POLL_LIMIT) return;
      timer = setTimeout(async () => { timer = null; polls += 1; await act({ action: 'refresh' }, true); }, POLL_MS);
    }

    async function act(body, quiet) {
      if (busy) return;
      busy = true; enable();
      if (!quiet) message.textContent = 'Working…';
      try {
        const doc = await api(ROUTE, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
        busy = false;
        paint(doc);
      } catch (_) {
        busy = false; enable();
        message.textContent = 'Could not reach FlyOnTheWall. Try again.';
        // A dropped refresh must not end the wait for a sign-in.
        schedule();
      }
    }

    keySave.addEventListener('click', async () => {
      const value = key.value;
      key.value = '';
      await act({ action: 'save_key', key: value });
    });
    keyForget.addEventListener('click', () => act({ action: 'forget_key' }));
    ack.addEventListener('change', enable);
    connect.addEventListener('click', () => { polls = 0; return act({ action: 'connect', acknowledge_egress: Boolean(ack.checked) }); });

    async function load() {
      retry.hidden = true;
      try { paint(await api(ROUTE)); }
      catch (error) {
        // A missing route means this server has no calendar settings; any
        // other failure stays visible so it can be retried.
        if (error && error.status === 404) { root.hidden = true; return; }
        root.hidden = false;
        message.textContent = 'Could not load calendar settings.';
        retry.hidden = false;
      }
    }
    retry.addEventListener('click', load);
    load();
    return root;
  },
};

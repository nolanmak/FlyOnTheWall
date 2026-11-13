// Calendar settings: Apple Calendar status, and an optional Google Calendar
// connection through Composio. The Composio key goes to the daemon once and
// is never shown again; the page only learns whether one is saved.
globalThis.FOTWCalendar = {
  settings(parent, api) {
    const ROUTE = '/api/settings/calendar';
    const POLL_MS = 3000, POLL_LIMIT = 100;
    const make = (tag, value, id, cls) => {
      const n = document.createElement(tag);
      if (value) n.textContent = value;
      if (id) n.id = id;
      if (cls) n.className = cls;
      return n;
    };
    const button = (label, id, cls) => { const b = make('button', label, id, cls); b.type = 'button'; return b; };

    const root = make('details', '', 'calendar-settings', 'gh-settings cal-settings'); root.hidden = true;
    root.appendChild(make('summary', 'Calendars'));
    const body = root.appendChild(make('div', '', '', 'cal-body'));
    body.appendChild(make('p', 'Recordings are named after the calendar event they overlap.', '', 'cal-lede'));

    // Apple Calendar
    const appleCard = body.appendChild(make('section', '', '', 'cal-card'));
    const appleHead = appleCard.appendChild(make('div', '', '', 'cal-card-head'));
    appleHead.appendChild(make('h3', 'Apple Calendar', '', 'cal-title'));
    const apple = appleHead.appendChild(make('span', '', 'calendar-apple', 'cal-pill'));
    appleCard.appendChild(make('p', 'Includes any Google accounts added in System Settings › Internet Accounts.', '', 'cal-sub'));

    // Google through Composio
    const googleCard = body.appendChild(make('section', '', '', 'cal-card'));
    const googleHead = googleCard.appendChild(make('div', '', '', 'cal-card-head'));
    googleHead.appendChild(make('h3', 'Google Calendar', '', 'cal-title'));
    googleHead.appendChild(make('span', 'via Composio', '', 'cal-via'));
    googleCard.appendChild(make('p', 'Event titles and times pass through Composio (backend.composio.dev) when a recording is saved. Audio, transcripts and summaries never do.', '', 'cal-sub'));

    const keySaved = googleCard.appendChild(make('div', '', '', 'cal-key-saved'));
    const keyState = keySaved.appendChild(make('span', '', 'calendar-key-state', 'cal-key-state'));
    const keyForget = keySaved.appendChild(button('Forget key', 'calendar-key-forget', 'cal-ghost'));
    const keyRow = googleCard.appendChild(make('div', '', 'calendar-key-row', 'cal-key-row'));
    const key = keyRow.appendChild(make('input', '', 'calendar-key', 'cal-input'));
    key.type = 'password'; key.autocomplete = 'off'; key.spellcheck = false; key.maxLength = 512;
    key.placeholder = 'Composio project API key (ak_…)';
    key.setAttribute('aria-label', 'Composio API key');
    const keySave = keyRow.appendChild(button('Save key', 'calendar-key-save', 'cal-primary'));

    const accounts = googleCard.appendChild(make('ul', '', 'calendar-accounts', 'cal-accounts'));

    const ackLabel = googleCard.appendChild(make('label', '', '', 'cal-ack'));
    const ack = ackLabel.appendChild(make('input', '', 'calendar-ack')); ack.type = 'checkbox';
    ackLabel.appendChild(make('span', 'I understand event titles and times pass through Composio.'));
    const actions = googleCard.appendChild(make('div', '', '', 'cal-actions'));
    const connect = actions.appendChild(button('Connect Google Calendar', 'calendar-connect', 'cal-primary'));
    const signin = actions.appendChild(make('a', 'Continue to Google sign-in →', 'calendar-signin', 'cal-link'));
    signin.target = '_blank'; signin.rel = 'noopener noreferrer'; signin.hidden = true;

    const message = body.appendChild(make('p', '', 'calendar-status', 'cal-status')); message.setAttribute('role', 'status');
    const retry = body.appendChild(button('Try again', 'calendar-retry', 'cal-ghost')); retry.hidden = true;
    parent.appendChild(root);

    let current = null, busy = false, polls = 0, timer = null, named = false;
    const pills = { active: 'Connected', pending: 'Signing in…', failed: 'Failed' };
    const notes = { pending: 'Waiting for Google sign-in…', failed: 'Sign-in failed or expired. Disconnect and connect again.' };

    function accountRow(account, i, seen) {
      const label = account.email || ('Google account ' + (i + 1));
      const item = make('li', '', '', 'cal-account');
      item.appendChild(make('span', (account.email || 'G').charAt(0).toUpperCase(), '', 'cal-avatar')).setAttribute('aria-hidden', 'true');
      const text = item.appendChild(make('div', '', '', 'cal-account-text'));
      text.appendChild(make('span', label, '', 'cal-email'));
      const duplicate = account.email && seen.has(account.email);
      if (account.email) seen.add(account.email);
      const note = duplicate ? 'Also connected above. You can disconnect this one.' : notes[account.status];
      if (note) text.appendChild(make('span', note, '', 'cal-meta'));
      item.appendChild(make('span', pills[account.status] || account.status, '', 'cal-pill is-' + account.status));
      const drop = item.appendChild(button('Disconnect', '', 'cal-ghost'));
      drop.setAttribute('aria-label', 'Disconnect ' + label);
      drop.addEventListener('click', () => act({ action: 'disconnect', id: account.id }));
      return item;
    }

    function paint(doc) {
      current = doc;
      root.hidden = false;
      retry.hidden = true;
      const appleOn = doc.apple_access === 'granted';
      apple.textContent = appleOn ? 'On' : (doc.apple_access || 'unknown');
      apple.className = 'cal-pill ' + (appleOn ? 'is-active' : 'is-pending');
      keyRow.hidden = Boolean(doc.composio_key_saved);
      keySaved.hidden = !doc.composio_key_saved;
      keyForget.hidden = !doc.composio_key_saved;
      keyState.textContent = doc.composio_key_saved ? 'Composio key saved in your keychain.' : 'Paste a Composio API key to connect Google accounts.';
      if (doc.composio_acknowledged) ack.checked = true;
      ackLabel.hidden = Boolean(doc.composio_acknowledged) || !doc.composio_key_saved;
      const list = doc.google_accounts || [];
      connect.textContent = list.length ? 'Connect another Google account' : 'Connect Google Calendar';
      actions.hidden = !doc.composio_key_saved;
      const seen = new Set();
      accounts.replaceChildren(...list.map((account, i) => accountRow(account, i, seen)));
      accounts.hidden = list.length === 0;
      const url = typeof doc.redirect_url === 'string' && doc.redirect_url.startsWith('https://') ? doc.redirect_url : '';
      if (url) { signin.href = url; signin.hidden = false; }
      if (!list.some(a => a.status === 'pending')) signin.hidden = true;
      message.textContent = doc.error || (url ? 'Sign in on the Google page, then come back here.' : '');
      enable();
      schedule();
      // Accounts connected before addresses were recorded get named once.
      if (!named && doc.composio_key_saved && list.some(a => a.status === 'active' && !a.email)) {
        named = true;
        setTimeout(() => act({ action: 'refresh' }, true), 0);
      }
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

    async function act(request, quiet) {
      if (busy) return;
      busy = true; enable();
      if (!quiet) message.textContent = 'Working…';
      try {
        const doc = await api(ROUTE, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(request) });
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

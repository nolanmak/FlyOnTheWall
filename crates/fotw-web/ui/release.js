// Attendee release (#326 5.1, 5.7): the settings panel, the Outbox and the
// per-meeting release line. The Outbox only ever shows the copy the daemon
// stored (subject, body, attachment); nothing here renders a release.
(function () {
  const make = (tag, value, id, cls) => {
    const n = document.createElement(tag);
    if (value) n.textContent = value;
    if (id) n.id = id;
    if (cls) n.className = cls;
    return n;
  };
  const button = (label, id, cls) => { const b = make('button', label, id, cls); b.type = 'button'; return b; };
  const field = (parent, label, input) => {
    const row = parent.appendChild(make('label', '', '', 'gh-field'));
    row.appendChild(make('span', label));
    row.appendChild(input);
    return input;
  };
  const input = (id, type, placeholder) => {
    const n = make('input', '', id);
    n.type = type; n.autocomplete = 'off'; n.spellcheck = false;
    if (placeholder) n.placeholder = placeholder;
    return n;
  };
  const post = body => ({ method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });

  // Reason codes (D10) as words. An unknown code shows as itself.
  const REASONS = {
    release_off: 'attendee release is off',
    before_cutoff: 'recorded before release was turned on',
    not_ready: 'the meeting is not ready yet',
    no_confident_match: 'no confident calendar match',
    no_recipients: 'no invitees in your allowed domains',
    too_long: 'the meeting is too long to release',
    window_passed: 'more than 7 days since the meeting ended',
    event_not_found: 'the calendar event was not found',
    calendar_unavailable: 'the calendar could not be checked; it will be tried again',
    model_failed: 'the summary or review model failed',
    body_too_large: 'the email would be too large; cut more of the meeting',
    send_refused: 'Gmail refused the send',
    review_required: 'approval mode: every release waits for you',
    has_findings: 'the review flagged something',
    first_time_recipient: 'first release to at least one recipient',
    many_recipients: 'more than 50 recipients',
    mostly_omitted: 'most of the meeting was cut',
    edited: 'you edited the cuts',
    reenabled_while_scheduled: 'release was turned back on while this was scheduled',
    delayed_while_paused: 'sending was paused when this was due',
    mode_changed: 'the release mode changed',
    auto_locked: 'auto mode is locked',
    inputs_changed: 'the meeting changed after this was prepared',
    send_outcome_unknown: 'may already have been sent; check before resending',
    by_user: 'dropped by you',
    wrong_event: 'matched to the wrong calendar event',
  };
  // What each worker pause stops (#326 D10): gws only sending, the engine
  // only preparing, unreadable settings everything.
  const PAUSES = {
    gws_unavailable: 'Sending is paused: gws could not be started or the sender is not verified.',
    engine_unavailable: 'Preparing is paused: no summary engine is available under the current summary policy.',
    settings_unreadable: 'Release is paused: the saved release settings could not be read. Save complete settings to resume.',
  };
  const reasonText = code => REASONS[code] || String(code);
  const pauseText = code => PAUSES[code] || ('Release is paused: ' + String(code) + '.');
  const two = n => String(n).padStart(2, '0');
  const clock = ms => { const d = new Date(ms); return two(d.getHours()) + ':' + two(d.getMinutes()); };
  const stamp = ms => {
    const d = new Date(ms);
    return d.getFullYear() + '-' + two(d.getMonth() + 1) + '-' + two(d.getDate()) + ' ' + clock(ms);
  };
  const offset = ms => {
    const s = Math.floor((Number(ms) || 0) / 1000);
    return two(Math.floor(s / 3600)) + ':' + two(Math.floor(s / 60) % 60) + ':' + two(s % 60);
  };
  const people = n => n + (n === 1 ? ' person' : ' people');
  const reasonsOf = doc => (Array.isArray(doc.reasons) ? doc.reasons : []);

  // The 5.7 table.
  function lineText(status) {
    const reasons = reasonsOf(status);
    const n = Number(status.recipient_count) || 0;
    switch (status.state) {
      case 'not_eligible': return 'Not released: ' + reasonText(reasons[0] || 'release_off');
      case 'preparing': return 'Preparing release';
      case 'needs_review':
        return reasons.includes('send_outcome_unknown') ? 'May already have been sent; check before resending' : 'Waiting for your review';
      case 'scheduled': return 'Sending to ' + people(n) + (status.send_at ? ' at ' + clock(status.send_at) : '');
      case 'sending': return 'Sending';
      case 'held': return 'Held: ' + reasonText(reasons[0] || 'unknown');
      case 'sent': return 'Sent to ' + people(n) + (status.sent_at ? ', ' + stamp(status.sent_at) : '');
      case 'dropped':
        return reasons.includes('wrong_event') ? 'Not released: ' + reasonText('wrong_event') : 'Not released (dropped)';
      default: return 'Release: ' + String(status.state);
    }
  }

  // The matched calendar event, in local time. Rows created before times
  // were stored carry the title only.
  function matchText(doc) {
    const start = Number(doc.event_start_ms), end = Number(doc.event_end_ms);
    let when = '';
    if (doc.event_start_ms != null && Number.isFinite(start)) {
      when = stamp(start);
      if (doc.event_end_ms != null && Number.isFinite(end)) {
        const a = new Date(start), b = new Date(end);
        const sameDay = a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
        when += '–' + (sameDay ? clock(end) : stamp(end));
      }
    }
    return 'Matched to: ' + (doc.event_title || 'Untitled event') + (when ? ' · ' + when : '');
  }

  // Wrong meeting is offered only while nothing has gone out (the daemon
  // refuses it from sending, sent and dropped).
  const WRONG_EVENT_STATES = ['preparing', 'needs_review', 'scheduled', 'held'];

  // Fill `block` with "Matched to: …", its Wrong meeting button and an inline
  // confirmation; returns the status note. `prefix` keeps the meeting line's
  // ids apart from the Outbox detail's. `onReject` runs only after the person
  // confirms.
  function fillMatch(block, prefix, doc, onReject) {
    block.replaceChildren();
    block.appendChild(make('span', matchText(doc), prefix + '-match-text', 'meta'));
    const confirm = make('div', '', prefix + '-wrong-event-confirm', 'release-confirm');
    confirm.hidden = true;
    const status = make('p', '', prefix + '-match-status', 'gh-note');
    status.setAttribute('role', 'status');
    if (WRONG_EVENT_STATES.includes(doc.state)) {
      const wrong = block.appendChild(button('Wrong meeting', prefix + '-wrong-event', 'release-link release-wrong-event'));
      confirm.appendChild(make('p', 'Stop the release for this meeting? Nobody will be emailed.', prefix + '-wrong-event-question'));
      const yes = confirm.appendChild(button('Stop the release', prefix + '-wrong-event-yes'));
      const no = confirm.appendChild(button('Keep the release', prefix + '-wrong-event-no'));
      wrong.addEventListener('click', () => { confirm.hidden = false; no.focus?.(); });
      no.addEventListener('click', () => { confirm.hidden = true; });
      yes.addEventListener('click', () => { confirm.hidden = true; return onReject(status); });
    }
    block.appendChild(confirm);
    block.appendChild(status);
    return status;
  }

  const refusal = error => (error && typeof error.code === 'string' && error.code ? error.code : 'Could not reach FlyOnTheWall. Try again.');

  function line(id, parent, api) {
    const ROUTE = '/api/meetings/' + encodeURIComponent(id) + '/release';
    const root = make('p', '', 'release-line', 'meta release-line');
    root.setAttribute('role', 'status');
    root.hidden = true;
    parent.appendChild(root);
    const match = make('div', '', 'release-line-match', 'release-match');
    match.hidden = true;
    parent.appendChild(match);
    let lastNote = null;

    function paint(status) {
      // With release off every meeting would say so; that is noise, not news.
      const off = status.state === 'not_eligible' && reasonsOf(status).every(r => r === 'release_off');
      root.textContent = lineText(status);
      root.hidden = off;
      // Only a release row names an event.
      if (off || !status.event_title) {
        match.replaceChildren();
        match.hidden = true;
        return;
      }
      lastNote = fillMatch(match, 'release-line', status, async note => {
        note.textContent = 'Working…';
        try {
          await api('/api/release/outbox/' + encodeURIComponent(id), post({ action: 'wrong_event' }));
          await load();
        } catch (error) {
          // The row probably moved on: say why, then show where it is now.
          const why = refusal(error);
          await load();
          (lastNote || note).textContent = why;
        }
      });
      match.hidden = false;
    }

    async function load() {
      try {
        const status = await api(ROUTE);
        if (root.isConnected === false) return;
        paint(status);
      } catch (_) {
        root.hidden = true;
        match.hidden = true;
      }
    }
    load();
    return root;
  }

  function settings(parent, api) {
    const ROUTE = '/api/settings/release';
    const root = make('details', '', 'release-settings', 'gh-settings release-settings');
    root.hidden = true;
    root.appendChild(make('summary', 'Attendee release'));
    root.appendChild(make('p', 'Email each meeting’s reviewed notes to its invitees in your allowed domains, through your own gws login. Nothing is sent until this is enabled.', '', 'gh-note'));
    const paused = root.appendChild(make('p', '', 'release-settings-paused', 'release-banner'));
    paused.setAttribute('role', 'status');
    paused.hidden = true;

    const enabledRow = root.appendChild(make('label', '', '', 'gh-check'));
    const enabled = enabledRow.appendChild(input('release-enabled', 'checkbox'));
    enabledRow.appendChild(make('span', ' Enable attendee release'));

    const mode = field(root, 'Mode', make('select', '', 'release-mode'));
    const approval = mode.appendChild(make('option', 'Approval: every release waits for you'));
    approval.value = 'approval';
    const auto = mode.appendChild(make('option', 'Auto: clean releases send after the delay'));
    auto.value = 'auto';
    const autoNote = root.appendChild(make('p', 'Auto unlocks after 20 of your review decisions with the current summary engine, at least 80% of them approved unchanged.', 'release-auto-note', 'gh-note'));

    const domains = field(root, 'Allowed domains', input('release-domains', 'text', 'example.com, example.org'));
    root.appendChild(make('p', 'Exact domains, lowercase, separated by commas. Subdomains do not match. Only invitees in these domains receive anything.', '', 'gh-note'));
    const profile = field(root, 'gws profile folder', input('release-gws-profile', 'text', '~/.config/gws'));
    const binary = field(root, 'gws binary (optional)', input('release-gws-binary', 'text', 'gws'));
    const delay = field(root, 'Send delay (minutes)', input('release-delay', 'number'));
    delay.min = '0'; delay.max = '120';
    const delayWarning = root.appendChild(make('p', 'A delay of 0 sends as soon as a release is approved: there is no chance to cancel.', 'release-delay-warning', 'gh-note release-warning'));
    const notify = field(root, 'Notify address (optional)', input('release-notify', 'email', 'you@example.com'));
    const sender = root.appendChild(make('p', '', 'release-sender', 'gh-note'));
    const save = root.appendChild(button('Save', 'release-save'));
    const message = root.appendChild(make('p', '', 'release-settings-status', 'gh-note'));
    message.setAttribute('role', 'status');
    parent.appendChild(root);

    let current = null;
    const paintDelay = () => { delayWarning.hidden = String(delay.value).trim() !== '0'; };

    function paint(doc) {
      current = doc;
      const s = doc.settings || {};
      root.hidden = false;
      enabled.checked = Boolean(s.enabled);
      const unlocked = doc.auto_unlocked === true;
      auto.disabled = !unlocked;
      autoNote.hidden = unlocked;
      mode.value = s.mode === 'auto' ? 'auto' : 'approval';
      domains.value = (s.allowed_domains || []).join(', ');
      profile.value = s.gws_profile_dir || '';
      binary.value = s.gws_binary || '';
      delay.value = String(s.send_delay_minutes ?? 15);
      notify.value = s.notify_address || '';
      sender.textContent = s.verified_sender ? 'Sends as ' + s.verified_sender + ' (verified).' : '';
      sender.hidden = !s.verified_sender;
      paused.textContent = doc.paused ? pauseText(doc.paused) : '';
      paused.hidden = !doc.paused;
      message.textContent = doc.error || '';
      paintDelay();
    }

    async function onSave() {
      const minutes = Number(String(delay.value).trim());
      if (String(delay.value).trim() === '' || !Number.isInteger(minutes) || minutes < 0 || minutes > 120) {
        message.textContent = 'Send delay must be a whole number from 0 to 120 minutes.';
        return;
      }
      const s = (current && current.settings) || {};
      const body = {
        enabled: Boolean(enabled.checked),
        mode: mode.value === 'auto' ? 'auto' : 'approval',
        allowed_domains: String(domains.value).split(/[\s,]+/).map(d => d.trim().toLowerCase()).filter(Boolean),
        gws_profile_dir: String(profile.value).trim(),
        gws_binary: String(binary.value).trim(),
        send_delay_minutes: minutes,
        notify_address: String(notify.value).trim(),
        // Server-set; sent back unchanged and ignored there.
        release_since_ms: s.release_since_ms || 0,
        verified_sender: s.verified_sender || '',
      };
      save.disabled = true;
      message.textContent = 'Saving…';
      try {
        const doc = await api(ROUTE, post(body));
        paint(doc);
        if (!doc.error) message.textContent = 'Saved.';
      } catch (_) {
        message.textContent = 'Could not reach FlyOnTheWall. Try again.';
      }
      save.disabled = false;
    }

    delay.addEventListener('input', paintDelay);
    delay.addEventListener('change', paintDelay);
    save.addEventListener('click', onSave);
    (async () => {
      try { paint(await api(ROUTE)); }
      catch (error) {
        if (error && error.status === 404) { root.hidden = true; return; }
        root.hidden = false;
        message.textContent = 'Could not load release settings.';
      }
    })();
    return root;
  }

  // A stored bullet, quoted from the stored body: the section heading, then
  // its `ordinal`-th list item (0-based).
  const SECTIONS = { summary: 'Summary', decisions: 'Decisions', action_items: 'Action items' };
  function storedBullet(body, section, ordinal) {
    const heading = (SECTIONS[section] || section).toLowerCase();
    const lines = String(body || '').split('\n');
    let inside = false, n = 0;
    for (const raw of lines) {
      const l = raw.trim();
      if (/^#{1,6}\s/.test(l) || /^\*\*[^*]+\*\*:?$/.test(l)) {
        inside = l.replace(/^#{1,6}\s+|\*\*|:$/g, '').trim().toLowerCase() === heading;
        n = 0;
        continue;
      }
      if (inside && /^([-*]|\d+\.)\s+/.test(l)) {
        if (n === ordinal) return l.replace(/^([-*]|\d+\.)\s+/, '');
        n += 1;
      }
    }
    return '';
  }

  function outbox(parent, api) {
    const LIST = '/api/release/outbox';
    const root = make('details', '', 'release-outbox', 'gh-settings release-outbox');
    root.hidden = true;
    const summary = root.appendChild(make('summary', 'Release outbox '));
    const badge = summary.appendChild(make('span', '', 'release-badge', 'release-badge'));
    badge.hidden = true;
    const paused = root.appendChild(make('p', '', 'release-paused', 'release-banner'));
    paused.setAttribute('role', 'status');
    paused.hidden = true;
    const items = root.appendChild(make('ul', '', 'release-items', 'release-items'));
    const listStatus = root.appendChild(make('p', '', 'release-list-status', 'gh-note'));
    const pane = root.appendChild(make('section', '', 'release-detail', 'release-detail'));
    pane.hidden = true;
    parent.appendChild(root);

    let openId = null, detailStatus = null;

    function paintList(doc) {
      const rows = (Array.isArray(doc.items) ? doc.items.slice() : []).sort((a, b) => (a.created_at || 0) - (b.created_at || 0));
      const count = Number(doc.badge) || 0;
      badge.textContent = String(count);
      badge.hidden = count === 0;
      paused.textContent = doc.paused ? pauseText(doc.paused) : '';
      paused.hidden = !doc.paused;
      items.replaceChildren(...rows.map(row => {
        const li = make('li', '', 'release-row-' + row.meeting_id, 'release-row is-' + row.state);
        const open = li.appendChild(button(row.event_title || 'Untitled meeting', 'release-open-' + row.meeting_id, 'release-open'));
        open.addEventListener('click', () => openDetail(row.meeting_id));
        li.appendChild(make('span', ' · ' + lineText(row) + ' · ' + people(Number(row.recipient_count) || 0) + (row.finding_count ? ' · ' + row.finding_count + ' finding' + (row.finding_count === 1 ? '' : 's') : ''), '', 'meta'));
        return li;
      }));
      listStatus.textContent = rows.length ? '' : 'Nothing waiting.';
      root.hidden = rows.length === 0 && !doc.paused && count === 0;
      if (openId && !rows.some(r => r.meeting_id === openId)) { openId = null; pane.hidden = true; }
    }

    async function loadList() {
      try { paintList(await api(LIST)); }
      catch (error) {
        if (error && error.status === 404) { root.hidden = true; return; }
        listStatus.textContent = 'Could not load the release outbox.';
      }
    }

    async function openDetail(id) {
      openId = id;
      try { paintDetail(await api(LIST + '/' + encodeURIComponent(id))); }
      catch (_) {
        pane.hidden = false;
        pane.replaceChildren(make('p', 'This release is no longer in the outbox.', 'release-detail-status', 'gh-note'));
        await loadList();
      }
    }

    async function act(id, body, status) {
      status.textContent = 'Working…';
      try {
        paintDetail(await api(LIST + '/' + encodeURIComponent(id), post(body)));
        await loadList();
      } catch (error) {
        // The row probably moved on (D10 compare-and-set): say why, then
        // show where it is now.
        const why = error && typeof error.code === 'string' && error.code ? error.code : 'Could not reach FlyOnTheWall. Try again.';
        await loadList();
        if (openId === id) {
          try { paintDetail(await api(LIST + '/' + encodeURIComponent(id))); } catch (_) { /* keep the old pane */ }
        }
        (detailStatus || status).textContent = why;
      }
    }

    function paintDetail(doc) {
      const row = doc.item || {};
      const id = row.meeting_id;
      const reasons = reasonsOf(row);
      const unknown = row.state === 'needs_review' && reasons.includes('send_outcome_unknown');
      pane.hidden = false;
      const nodes = [];
      nodes.push(make('h3', row.event_title || 'Untitled meeting'));
      nodes.push(make('p', lineText(row), 'release-detail-line', 'meta'));
      const match = make('div', '', 'release-detail-match', 'release-match');
      fillMatch(match, 'release-detail', row, note => act(id, { action: 'wrong_event' }, note));
      nodes.push(match);

      const to = make('ul', '', 'release-recipients', 'release-recipients');
      for (const r of doc.recipients || []) {
        to.appendChild(make('li', r.display_name ? r.display_name + ' <' + r.address + '>' : r.address));
      }
      nodes.push(make('h4', 'To')); nodes.push(to);

      const why = make('ul', '', 'release-reasons', 'release-reasons');
      for (const code of reasons) why.appendChild(make('li', reasonText(code)));
      nodes.push(make('h4', 'Why it is here')); nodes.push(why);

      const segments = Array.isArray(doc.segments) ? doc.segments : [];
      const segRows = new Map();
      const findings = make('ol', '', 'release-findings', 'release-findings');
      (doc.findings || []).forEach((f, i) => {
        const li = findings.appendChild(make('li', '', 'release-finding-' + i));
        li.appendChild(make('strong', String(f.category || '').replace(/_/g, ' ')));
        li.appendChild(make('span', ' ' + (f.reason || '')));
        for (const index of f.indices || []) {
          const seg = segments.find(s => s.index === index);
          const label = seg ? offset(seg.offset_ms) + ' · ' + seg.speaker + ': ' + seg.text : 'Excerpt ' + index;
          const link = li.appendChild(button(label, 'release-finding-' + i + '-excerpt-' + index, 'release-link'));
          link.addEventListener('click', () => {
            const target = segRows.get(index);
            if (target) { target.scrollIntoView?.({ block: 'center' }); target.className += ' is-flagged'; }
          });
        }
        if (Array.isArray(f.bullet)) {
          const [section, ordinal] = f.bullet;
          const quoted = storedBullet(doc.body, section, ordinal);
          li.appendChild(make('span', (SECTIONS[section] || section) + ', bullet ' + (Number(ordinal) + 1) + (quoted ? ': “' + quoted + '”' : ''), 'release-finding-' + i + '-bullet', 'release-bullet'));
        }
      });
      if ((doc.findings || []).length) { nodes.push(make('h4', 'Findings')); nodes.push(findings); }

      // The stored copy, verbatim.
      nodes.push(make('h4', 'Subject'));
      nodes.push(make('p', doc.subject || '', 'release-subject', 'release-stored'));
      nodes.push(make('h4', 'Email body'));
      nodes.push(make('pre', doc.body || '', 'release-body', 'release-stored'));
      nodes.push(make('h4', 'Attachment'));
      nodes.push(make('pre', doc.attachment || '', 'release-attachment', 'release-stored'));

      // Every segment with its current cut; the boxes unlock for Edit cuts.
      const segList = make('div', '', 'release-segments', 'release-segments');
      const cuts = make('div', '', 'release-cuts', 'release-actions');
      cuts.hidden = true;
      const boxes = [];
      for (const seg of segments) {
        const label = segList.appendChild(make('label', '', 'release-seg-' + seg.index, 'release-seg' + (seg.retained ? '' : ' is-cut')));
        const box = label.appendChild(input('release-cut-' + seg.index, 'checkbox'));
        box.checked = seg.retained === true;
        box.disabled = true;
        label.appendChild(make('span', ' ' + offset(seg.offset_ms) + ' · ' + seg.speaker + ' — ' + seg.text));
        segRows.set(seg.index, label);
        boxes.push([seg.index, box]);
      }
      const saveCuts = cuts.appendChild(button('Save cuts', 'release-cuts-save'));
      const stopCuts = cuts.appendChild(button('Stop editing', 'release-cuts-cancel'));
      const editing = on => { cuts.hidden = !on; for (const [, box] of boxes) box.disabled = !on; };

      const actions = make('div', '', 'release-actions', 'release-actions');
      const confirm = make('div', '', 'release-send-again-confirm', 'release-confirm');
      confirm.hidden = true;
      confirm.appendChild(make('p', 'This email may already have been sent: the last attempt ended without a clear answer. Check the Sent folder of the sending account before sending it again.'));
      const yes = confirm.appendChild(button('Send again', 'release-send-again-yes'));
      const no = confirm.appendChild(button('Keep it', 'release-send-again-no'));
      const status = make('p', '', 'release-detail-status', 'gh-note');
      status.setAttribute('role', 'status');
      detailStatus = status;

      const add = (label, body, cls) => {
        const b = actions.appendChild(button(label, '', cls));
        if (body) b.addEventListener('click', () => act(id, body, status));
        return b;
      };
      const canEdit = row.state === 'needs_review' || (row.state === 'held' && reasons.includes('body_too_large'));
      if (row.state === 'needs_review' && !unknown) add('Approve & send', { action: 'approve' }, 'release-primary');
      if (unknown) {
        const again = add('Send again anyway', null, 'release-primary');
        again.addEventListener('click', () => { confirm.hidden = false; yes.focus?.(); });
      }
      if (row.state === 'scheduled') add('Cancel send', { action: 'cancel' });
      if (row.state === 'held') add('Retry', { action: 'retry' });
      if (canEdit) {
        const edit = add('Edit cuts');
        edit.disabled = unknown;
        if (unknown) edit.title = 'Edit cuts is not available while it may already have been sent.';
        edit.addEventListener('click', () => { if (!unknown) editing(true); });
      }
      if (row.state === 'needs_review' || row.state === 'held') add('Drop', { action: 'drop' });

      yes.addEventListener('click', () => { confirm.hidden = true; return act(id, { action: 'send_again' }, status); });
      no.addEventListener('click', () => { confirm.hidden = true; });
      saveCuts.addEventListener('click', () => {
        const retained = boxes.filter(([, box]) => box.checked).map(([index]) => index);
        if (!retained.length) { status.textContent = 'Keep at least one segment, or drop the release.'; return; }
        return act(id, { action: 'edit_cuts', retained }, status);
      });
      stopCuts.addEventListener('click', () => {
        for (const seg of segments) { const hit = boxes.find(([i]) => i === seg.index); if (hit) hit[1].checked = seg.retained === true; }
        editing(false);
      });

      nodes.push(actions, confirm, status, make('h4', 'Transcript segments (ticked = sent)'), segList, cuts);
      pane.replaceChildren(...nodes);
    }

    root.addEventListener('toggle', () => { if (root.open) loadList(); });
    loadList();
    return root;
  }

  globalThis.FOTWRelease = { settings, outbox, line, lineText };
})();

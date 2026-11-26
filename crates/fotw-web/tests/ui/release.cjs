// Attendee release UI (#332): settings panel, Outbox and the meeting line.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function rig() {
  class Node {
    constructor(tag) { this.tagName = tag; this.children = []; this.listeners = {}; this.hidden = false; this.disabled = false; this.textContent = ''; this.value = ''; this.checked = false; this.attrs = {}; }
    appendChild(n) { n.parentNode = this; this.children.push(n); return n; }
    replaceChildren(...nodes) { this.children = []; nodes.forEach(n => this.appendChild(n)); }
    setAttribute(k, v) { this.attrs[k] = v; }
    getAttribute(k) { return this.attrs[k]; }
    addEventListener(k, fn) { (this.listeners[k] ||= []).push(fn); }
    fire(k) { return Promise.all((this.listeners[k] || []).map(fn => fn())); }
    async click() { if (!this.disabled) return this.fire('click'); }
    focus() { this.focused = true; }
    scrollIntoView() { this.scrolled = true; }
  }
  const body = new Node('body'), requests = [];
  let respond = () => ({});
  const ctx = vm.createContext({ document: { createElement: tag => new Node(tag) }, setTimeout: () => 0, clearTimeout: () => {}, Date });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../ui/release.js'), 'utf8'), ctx);
  const api = async (url, options) => {
    requests.push({ url, method: options?.method || 'GET', body: options?.body ? JSON.parse(options.body) : undefined });
    const answer = respond(url, options);
    if (answer instanceof Error) throw answer;
    return JSON.parse(JSON.stringify(answer));
  };
  const all = (n = body) => [n, ...n.children.flatMap(c => all(c))];
  const byId = id => all().find(n => n.id === id);
  const visible = n => { for (let x = n; x; x = x.parentNode) if (x.hidden) return false; return true; };
  const buttons = () => all().filter(n => n.tagName === 'button' && visible(n));
  const button = label => buttons().find(b => b.textContent === label);
  return {
    body, requests, respond: fn => { respond = fn; },
    release: ctx.FOTWRelease, api, all, byId, visible, buttons, button,
    text: (n = body) => all(n).filter(visible).map(x => x.textContent).join(' | '),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
const posts = w => w.requests.filter(r => r.method === 'POST');

// --------------------------------------------------------------- settings

function settingsDoc(extra = {}, settings = {}) {
  return {
    settings: { enabled: false, mode: 'approval', allowed_domains: [], gws_profile_dir: '', gws_binary: '', send_delay_minutes: 15, notify_address: '', release_since_ms: 0, verified_sender: '', ...settings },
    auto_unlocked: false,
    ...extra,
  };
}

async function settings(doc = settingsDoc()) {
  const w = rig();
  w.respond(() => doc);
  const root = w.release.settings(w.body, w.api);
  await flush();
  return { w, root };
}

test('settings shows every field from the stored copy', async () => {
  const { w, root } = await settings(settingsDoc({}, { enabled: true, allowed_domains: ['example.com', 'example.org'], gws_profile_dir: '/Users/example/.gws', gws_binary: '/opt/gws', send_delay_minutes: 30, notify_address: 'owner@example.com', verified_sender: 'owner@example.com' }));
  assert.equal(root.hidden, false);
  assert.equal(w.byId('release-enabled').checked, true);
  assert.equal(w.byId('release-mode').value, 'approval');
  assert.equal(w.byId('release-domains').value, 'example.com, example.org');
  assert.equal(w.byId('release-gws-profile').value, '/Users/example/.gws');
  assert.equal(w.byId('release-gws-binary').value, '/opt/gws');
  assert.equal(w.byId('release-delay').value, '30');
  assert.equal(w.byId('release-notify').value, 'owner@example.com');
  assert.match(w.byId('release-sender').textContent, /owner@example\.com/);
});

test('auto stays disabled with an explanation until it unlocks', async () => {
  const locked = await settings();
  const auto = locked.w.all().find(n => n.tagName === 'option' && n.value === 'auto');
  assert.equal(auto.disabled, true);
  assert.equal(locked.w.visible(locked.w.byId('release-auto-note')), true);
  assert.match(locked.w.byId('release-auto-note').textContent, /20/);

  const open = await settings(settingsDoc({ auto_unlocked: true }));
  const unlocked = open.w.all().find(n => n.tagName === 'option' && n.value === 'auto');
  assert.equal(unlocked.disabled, false);
  assert.equal(open.w.visible(open.w.byId('release-auto-note')), false);
});

test('a send delay of 0 warns that it removes the chance to cancel', async () => {
  const { w } = await settings();
  const warning = w.byId('release-delay-warning');
  assert.equal(w.visible(warning), false);
  w.byId('release-delay').value = '0';
  await w.byId('release-delay').fire('input');
  assert.equal(w.visible(warning), true);
  assert.match(warning.textContent, /chance to cancel/);
  w.byId('release-delay').value = '5';
  await w.byId('release-delay').fire('input');
  assert.equal(w.visible(warning), false);
  // A stored 0 warns on load too.
  const stored = await settings(settingsDoc({}, { send_delay_minutes: 0 }));
  assert.equal(stored.w.visible(stored.w.byId('release-delay-warning')), true);
});

test('save posts the form and a refusal shows the error with the stored copy', async () => {
  const { w } = await settings();
  w.byId('release-enabled').checked = true;
  w.byId('release-domains').value = ' Example.com,\nexample.org ';
  w.byId('release-gws-profile').value = '/Users/example/.gws';
  w.byId('release-delay').value = '10';
  w.byId('release-notify').value = 'owner@example.com';
  w.respond(() => settingsDoc({ error: 'Enabling needs a passing gws check.' }));
  await w.byId('release-save').click();
  await flush();
  const [post] = posts(w);
  assert.equal(post.url, '/api/settings/release');
  assert.equal(post.body.enabled, true);
  assert.deepEqual(post.body.allowed_domains, ['example.com', 'example.org']);
  assert.equal(post.body.gws_profile_dir, '/Users/example/.gws');
  assert.equal(post.body.send_delay_minutes, 10);
  assert.equal(post.body.notify_address, 'owner@example.com');
  assert.equal(post.body.mode, 'approval');
  assert.equal(w.byId('release-settings-status').textContent, 'Enabling needs a passing gws check.');
  assert.equal(w.byId('release-enabled').checked, false, 'the form shows what is still stored');
});

test('an out-of-range delay is refused before it is sent', async () => {
  const { w } = await settings();
  w.byId('release-delay').value = '121';
  await w.byId('release-save').click();
  await flush();
  assert.equal(posts(w).length, 0);
  assert.match(w.byId('release-settings-status').textContent, /0 to 120/);
});

test('settings shows the pause banner and the unavailable error', async () => {
  const paused = await settings(settingsDoc({ paused: 'gws_unavailable' }));
  assert.equal(paused.w.visible(paused.w.byId('release-settings-paused')), true);
  assert.match(paused.w.byId('release-settings-paused').textContent, /paused/i);
  const off = await settings(settingsDoc({ error: 'attendee release is not available' }));
  assert.equal(off.w.byId('release-settings-status').textContent, 'attendee release is not available');
  assert.equal(off.w.visible(off.w.byId('release-settings-paused')), false);
});

// ----------------------------------------------------------- meeting line

async function line(status) {
  const w = rig();
  w.respond(() => status);
  const root = w.release.line('m1', w.body, w.api);
  await flush();
  return { w, root };
}

test('the meeting line asks the status endpoint for this meeting', async () => {
  const { w } = await line({ state: 'preparing', reasons: [], recipient_count: 0 });
  assert.equal(w.requests[0].url, '/api/meetings/m1/release');
});

test('every meeting status maps to its line', async () => {
  const at = new Date(2026, 0, 2, 9, 5).getTime();
  const cases = [
    [{ state: 'not_eligible', reasons: ['no_recipients'], recipient_count: 0 }, /^Not released: .*allowed domains/],
    [{ state: 'not_eligible', reasons: ['window_passed'], recipient_count: 0 }, /^Not released: .*7 days/],
    [{ state: 'preparing', reasons: [], recipient_count: 2 }, /^Preparing release$/],
    [{ state: 'needs_review', reasons: ['review_required'], recipient_count: 2 }, /^Waiting for your review$/],
    [{ state: 'needs_review', reasons: ['has_findings', 'send_outcome_unknown'], recipient_count: 2 }, /^May already have been sent; check before resending$/],
    [{ state: 'scheduled', reasons: [], recipient_count: 3, send_at: at }, /^Sending to 3 people at 09:05$/],
    [{ state: 'sending', reasons: [], recipient_count: 3 }, /^Sending$/],
    [{ state: 'held', reasons: ['body_too_large'], recipient_count: 3 }, /^Held: .*too large/],
    [{ state: 'held', reasons: ['send_refused'], recipient_count: 3 }, /^Held: .*refused/],
    [{ state: 'sent', reasons: [], recipient_count: 4, sent_at: at }, /^Sent to 4 people, .*09:05$/],
    [{ state: 'dropped', reasons: ['by_user'], recipient_count: 3 }, /^Not released \(dropped\)$/],
  ];
  for (const [status, expected] of cases) {
    const { root } = await line(status);
    assert.equal(root.hidden, false, status.state);
    assert.match(root.textContent, expected, JSON.stringify(status));
  }
  const one = await line({ state: 'scheduled', reasons: [], recipient_count: 1, send_at: at });
  assert.match(one.root.textContent, /^Sending to 1 person at/);
});

test('an unknown reason code still shows, as the code', async () => {
  const { root } = await line({ state: 'held', reasons: ['brand_new_code'], recipient_count: 1 });
  assert.equal(root.textContent, 'Held: brand_new_code');
});

test('the line stays hidden while release is off or the route is missing', async () => {
  const off = await line({ state: 'not_eligible', reasons: ['release_off'], recipient_count: 0 });
  assert.equal(off.root.hidden, true);
  const missing = await line(Object.assign(new Error('nf'), { status: 404 }));
  assert.equal(missing.root.hidden, true);
});

// ------------------------------------------------------------------ outbox

function item(extra = {}) {
  return { meeting_id: 'm1', event_title: 'Weekly sync', state: 'needs_review', reasons: ['has_findings'], recipient_count: 2, finding_count: 2, created_at: 2000, send_at: null, ...extra };
}

function detail(itemExtra = {}, extra = {}) {
  return {
    item: item(itemExtra),
    recipients: [{ address: 'ada@example.com', display_name: 'Ada Example' }, { address: 'lin@example.org', display_name: null }],
    subject: 'STORED subject: Weekly sync',
    body: 'STORED body\n\n## Summary\n- First stored bullet\n- Second stored bullet\n',
    attachment: '# Notes: Weekly sync\nSTORED attachment',
    findings: [
      { category: 'personal', indices: [1], bullet: null, reason: 'Mentions a health matter' },
      { category: 'confidential', indices: [], bullet: ['summary', 1], reason: 'Quotes a price' },
    ],
    segments: [
      { index: 0, offset_ms: 0, speaker: 'Ada', text: 'Hello all', retained: true },
      { index: 1, offset_ms: 65000, speaker: 'Lin', text: 'About my doctor visit', retained: true },
      { index: 2, offset_ms: 130000, speaker: 'Ada', text: 'Off the record', retained: false },
    ],
    ...extra,
  };
}

function server(w, { list, one, act }) {
  w.respond((url, options) => {
    if (url === '/api/release/outbox') return list();
    if (options?.method === 'POST') return act(JSON.parse(options.body));
    return one(url);
  });
}

async function outbox({ items = [item()], paused = null, badge, one = () => detail(), act = () => detail() } = {}) {
  const w = rig();
  const list = () => ({ items, badge: badge ?? items.filter(i => i.state !== 'scheduled').length, paused });
  server(w, { list, one, act });
  const root = w.release.outbox(w.body, w.api);
  await flush();
  return { w, root };
}

async function open(w, id = 'm1') {
  await w.byId('release-open-' + id).click();
  await flush();
}

test('the outbox lists rows oldest first with the badge and the pause banner', async () => {
  const { w, root } = await outbox({
    items: [item({ meeting_id: 'late', event_title: 'Later meeting', created_at: 9000, state: 'held', reasons: ['model_failed'] }), item({ meeting_id: 'early', event_title: 'Earlier meeting', created_at: 1000 }), item({ meeting_id: 'mid', event_title: 'Scheduled meeting', created_at: 5000, state: 'scheduled', reasons: [], send_at: 99 })],
    paused: 'gws_unavailable',
  });
  assert.equal(root.hidden, false);
  const rows = w.byId('release-items').children;
  assert.deepEqual(rows.map(r => r.id), ['release-row-early', 'release-row-mid', 'release-row-late']);
  assert.equal(w.byId('release-badge').textContent, '2');
  assert.equal(w.visible(w.byId('release-badge')), true);
  assert.equal(w.visible(w.byId('release-paused')), true);
  assert.match(w.byId('release-paused').textContent, /paused/i);
});

test('an empty outbox with nothing paused stays out of the way', async () => {
  const { w, root } = await outbox({ items: [] });
  assert.equal(root.hidden, true);
  assert.equal(w.visible(w.byId('release-badge')), false);
});

test('the detail shows the stored copy, frozen recipients, every reason and linked findings', async () => {
  const { w } = await outbox({ one: () => detail({ reasons: ['has_findings', 'first_time_recipient'] }) });
  await open(w);
  assert.equal(w.requests.at(-1).url, '/api/release/outbox/m1');
  // Stored copy, verbatim: no fresh render.
  assert.equal(w.byId('release-subject').textContent, 'STORED subject: Weekly sync');
  assert.equal(w.byId('release-body').textContent, detail().body);
  assert.equal(w.byId('release-attachment').textContent, detail().attachment);
  const recipients = w.text(w.byId('release-recipients'));
  assert.match(recipients, /Ada Example/);
  assert.match(recipients, /ada@example\.com/);
  assert.match(recipients, /lin@example\.org/);
  const reasons = w.text(w.byId('release-reasons'));
  assert.match(reasons, /review flagged/);
  assert.match(reasons, /first release/i);
  // A finding with indices links to its excerpt.
  const excerpt = w.byId('release-finding-0-excerpt-1');
  assert.match(excerpt.textContent, /00:01:05 · Lin/);
  assert.match(excerpt.textContent, /doctor visit/);
  await excerpt.click();
  assert.equal(w.byId('release-seg-1').scrolled, true);
  // A finding with a bullet links to that stored bullet.
  const bullet = w.byId('release-finding-1-bullet');
  assert.match(bullet.textContent, /Summary/);
  assert.match(bullet.textContent, /Second stored bullet/);
  assert.match(w.text(w.byId('release-findings')), /Mentions a health matter/);
  assert.match(w.text(w.byId('release-findings')), /Quotes a price/);
});

test('needs_review offers Approve & send, Edit cuts and Drop', async () => {
  const sent = [];
  const { w } = await outbox({ act: body => { sent.push(body); return detail({ state: 'scheduled', reasons: [], send_at: 1 }); } });
  await open(w);
  assert.ok(w.button('Approve & send'));
  assert.ok(w.button('Edit cuts'));
  assert.ok(w.button('Drop'));
  assert.equal(w.button('Cancel send'), undefined);
  assert.equal(w.button('Retry'), undefined);
  assert.equal(w.button('Send again anyway'), undefined);
  await w.button('Approve & send').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'approve' }]);
  assert.equal(posts(w)[0].url, '/api/release/outbox/m1');
  assert.ok(w.button('Cancel send'), 'repainted from the response');
});

test('Drop posts drop', async () => {
  const sent = [];
  const { w } = await outbox({ act: body => { sent.push(body); return detail(); } });
  await open(w);
  await w.button('Drop').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'drop' }]);
});

test('Edit cuts toggles segments and saves the retained set', async () => {
  const sent = [];
  const { w } = await outbox({ act: body => { sent.push(body); return detail({ reasons: ['edited'] }); } });
  await open(w);
  assert.equal(w.visible(w.byId('release-cuts')), false);
  await w.button('Edit cuts').click();
  assert.equal(w.visible(w.byId('release-cuts')), true);
  assert.equal(w.byId('release-cut-0').checked, true);
  assert.equal(w.byId('release-cut-2').checked, false);
  w.byId('release-cut-1').checked = false;
  w.byId('release-cut-2').checked = true;
  await w.button('Save cuts').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'edit_cuts', retained: [0, 2] }]);
});

test('Edit cuts refuses an empty retained set', async () => {
  const sent = [];
  const { w } = await outbox({ act: body => { sent.push(body); return detail(); } });
  await open(w);
  await w.button('Edit cuts').click();
  for (const i of [0, 1, 2]) w.byId('release-cut-' + i).checked = false;
  await w.button('Save cuts').click();
  await flush();
  assert.equal(sent.length, 0);
  assert.match(w.byId('release-detail-status').textContent, /at least one/i);
});

test('scheduled offers only Cancel send', async () => {
  const sent = [];
  const { w } = await outbox({ items: [item({ state: 'scheduled', reasons: [], send_at: 5 })], one: () => detail({ state: 'scheduled', reasons: [], send_at: 5 }), act: body => { sent.push(body); return detail({ reasons: ['review_required'] }); } });
  await open(w);
  assert.equal(w.button('Approve & send'), undefined);
  assert.equal(w.button('Edit cuts'), undefined);
  assert.equal(w.button('Drop'), undefined);
  await w.button('Cancel send').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'cancel' }]);
});

test('held offers Retry and Drop, and Edit cuts only for body_too_large', async () => {
  const sent = [];
  const failed = await outbox({ items: [item({ state: 'held', reasons: ['model_failed'] })], one: () => detail({ state: 'held', reasons: ['model_failed'] }), act: body => { sent.push(body); return detail({ state: 'preparing', reasons: [] }); } });
  await open(failed.w);
  assert.ok(failed.w.button('Drop'));
  assert.equal(failed.w.button('Edit cuts'), undefined);
  assert.equal(failed.w.button('Approve & send'), undefined);
  await failed.w.button('Retry').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'retry' }]);

  const large = await outbox({ items: [item({ state: 'held', reasons: ['body_too_large'] })], one: () => detail({ state: 'held', reasons: ['body_too_large'] }) });
  await open(large.w);
  assert.ok(large.w.button('Edit cuts'));
  assert.equal(large.w.button('Edit cuts').disabled, false);
  assert.ok(large.w.button('Retry'));
});

test('send_outcome_unknown needs a confirmation and disables Edit cuts', async () => {
  const sent = [];
  const unknown = { reasons: ['send_outcome_unknown'] };
  const { w } = await outbox({ items: [item(unknown)], one: () => detail(unknown), act: body => { sent.push(body); return detail({ state: 'scheduled', reasons: [], send_at: 1 }); } });
  await open(w);
  assert.equal(w.button('Approve & send'), undefined, 'only the confirmed path resends');
  assert.ok(w.button('Drop'));
  assert.equal(w.button('Edit cuts').disabled, true);
  await w.button('Send again anyway').click();
  await flush();
  assert.equal(sent.length, 0, 'nothing is sent before the confirmation');
  const confirm = w.byId('release-send-again-confirm');
  assert.equal(w.visible(confirm), true);
  assert.match(confirm.textContent + w.text(confirm), /may already have been sent/i);
  await w.button('Keep it').click();
  assert.equal(w.visible(confirm), false);
  assert.equal(sent.length, 0);
  await w.button('Send again anyway').click();
  await w.byId('release-send-again-yes').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'send_again' }]);
});

test('a refused action shows why and reloads', async () => {
  const { w } = await outbox({ items: [item({ state: 'scheduled', reasons: [], send_at: 5 })], one: () => detail({ state: 'scheduled', reasons: [], send_at: 5 }), act: () => Object.assign(new Error('request failed'), { status: 409, code: 'This release has already started sending.' }) });
  await open(w);
  const before = w.requests.length;
  await w.button('Cancel send').click();
  await flush();
  assert.equal(w.byId('release-detail-status').textContent, 'This release has already started sending.');
  assert.ok(w.requests.slice(before).some(r => r.url === '/api/release/outbox'), 'the list reloads');
});

// ------------------------------------------------- matched calendar event

const START = new Date(2026, 0, 2, 9, 0).getTime();
const END = new Date(2026, 0, 2, 9, 30).getTime();

test('the meeting line names the matched event and its local times', async () => {
  const { w } = await line({ state: 'needs_review', reasons: ['review_required'], recipient_count: 2, event_title: 'Weekly sync', event_start_ms: START, event_end_ms: END });
  assert.equal(w.visible(w.byId('release-line-match')), true);
  assert.equal(w.byId('release-line-match-text').textContent, 'Matched to: Weekly sync · 2026-01-02 09:00–09:30');
  assert.ok(w.button('Wrong meeting'));
});

test('a row from before event times shows the title only', async () => {
  const { w } = await line({ state: 'scheduled', reasons: [], recipient_count: 2, send_at: START, event_title: 'Weekly sync' });
  assert.equal(w.byId('release-line-match-text').textContent, 'Matched to: Weekly sync');
});

test('no matched event, no match line', async () => {
  const { w } = await line({ state: 'not_eligible', reasons: ['no_confident_match'], recipient_count: 0 });
  assert.equal(w.visible(w.byId('release-line-match')), false);
  assert.equal(w.button('Wrong meeting'), undefined);
});

test('Wrong meeting shows for open states only', async () => {
  const status = state => ({ state, reasons: [], recipient_count: 2, event_title: 'Weekly sync', event_start_ms: START, event_end_ms: END });
  for (const state of ['preparing', 'needs_review', 'scheduled', 'held']) {
    const { w } = await line(status(state));
    assert.ok(w.button('Wrong meeting'), state);
  }
  for (const state of ['sending', 'sent', 'dropped']) {
    const { w } = await line(status(state));
    assert.equal(w.visible(w.byId('release-line-match')), true, state + ' still names the event');
    assert.equal(w.button('Wrong meeting'), undefined, state);
  }
});

test('Wrong meeting asks first, then posts wrong_event and repaints the line', async () => {
  const w = rig();
  let status = { state: 'scheduled', reasons: [], recipient_count: 2, send_at: START, event_title: 'Weekly sync', event_start_ms: START, event_end_ms: END };
  w.respond((url, options) => {
    if (options?.method === 'POST') {
      status = { ...status, state: 'dropped', reasons: ['wrong_event'], send_at: null };
      return detail({ state: 'dropped', reasons: ['wrong_event'] });
    }
    return status;
  });
  const root = w.release.line('m1', w.body, w.api);
  await flush();
  await w.button('Wrong meeting').click();
  await flush();
  assert.equal(posts(w).length, 0, 'nothing is posted before the confirmation');
  const confirm = w.byId('release-line-wrong-event-confirm');
  assert.equal(w.visible(confirm), true);
  assert.equal(w.byId('release-line-wrong-event-question').textContent, 'Stop the release for this meeting? Nobody will be emailed.');
  await w.byId('release-line-wrong-event-no').click();
  assert.equal(w.visible(confirm), false);
  assert.equal(posts(w).length, 0);

  await w.button('Wrong meeting').click();
  await w.byId('release-line-wrong-event-yes').click();
  await flush();
  const [post] = posts(w);
  assert.equal(post.url, '/api/release/outbox/m1');
  assert.deepEqual(post.body, { action: 'wrong_event' });
  assert.equal(root.textContent, 'Not released: matched to the wrong calendar event');
  assert.equal(w.button('Wrong meeting'), undefined);
});

test('a refused Wrong meeting says why', async () => {
  const w = rig();
  w.respond((url, options) => {
    if (options?.method === 'POST') return Object.assign(new Error('request failed'), { status: 409, code: 'This release changed state. Reload the Outbox.' });
    return { state: 'needs_review', reasons: [], recipient_count: 2, event_title: 'Weekly sync' };
  });
  w.release.line('m1', w.body, w.api);
  await flush();
  await w.button('Wrong meeting').click();
  await w.byId('release-line-wrong-event-yes').click();
  await flush();
  assert.equal(w.byId('release-line-match-status').textContent, 'This release changed state. Reload the Outbox.');
});

test('a wrong_event drop reads as the wrong calendar event', async () => {
  const { root } = await line({ state: 'dropped', reasons: ['wrong_event'], recipient_count: 2, event_title: 'Weekly sync' });
  assert.equal(root.textContent, 'Not released: matched to the wrong calendar event');
});

test('the outbox detail names the matched event and offers Wrong meeting', async () => {
  const sent = [];
  const times = { event_start_ms: START, event_end_ms: END };
  const { w } = await outbox({ items: [item(times)], one: () => detail(times), act: body => { sent.push(body); return detail({ ...times, state: 'dropped', reasons: ['wrong_event'] }); } });
  await open(w);
  assert.equal(w.byId('release-detail-match-text').textContent, 'Matched to: Weekly sync · 2026-01-02 09:00–09:30');
  await w.button('Wrong meeting').click();
  await flush();
  assert.equal(sent.length, 0, 'nothing is posted before the confirmation');
  assert.equal(w.visible(w.byId('release-detail-wrong-event-confirm')), true);
  assert.equal(w.byId('release-detail-wrong-event-question').textContent, 'Stop the release for this meeting? Nobody will be emailed.');
  await w.byId('release-detail-wrong-event-yes').click();
  await flush();
  assert.deepEqual(sent, [{ action: 'wrong_event' }]);
  assert.equal(w.byId('release-detail-line').textContent, 'Not released: matched to the wrong calendar event');
  assert.equal(w.button('Wrong meeting'), undefined, 'not offered once dropped');
});

test('the outbox offers Wrong meeting for scheduled and held rows too', async () => {
  for (const extra of [{ state: 'scheduled', reasons: [], send_at: 5 }, { state: 'held', reasons: ['model_failed'] }]) {
    const { w } = await outbox({ items: [item(extra)], one: () => detail(extra) });
    await open(w);
    assert.ok(w.button('Wrong meeting'), extra.state);
    assert.equal(w.byId('release-detail-match-text').textContent, 'Matched to: Weekly sync');
  }
});

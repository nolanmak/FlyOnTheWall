const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function rig() {
  class Node {
    constructor(tag) { this.tagName = tag; this.children = []; this.listeners = {}; this.hidden = false; this.disabled = false; this.textContent = ''; this.value = ''; this.checked = false; }
    appendChild(n) { n.parentNode = this; this.children.push(n); return n; }
    replaceChildren(...nodes) { this.children = []; nodes.forEach(n => this.appendChild(n)); }
    setAttribute(k, v) { this[k] = v; }
    addEventListener(k, fn) { this.listeners[k] = fn; }
    async click() { if (!this.disabled) return this.listeners.click?.(); }
    async change() { return this.listeners.change?.(); }
  }
  const body = new Node('body'), timers = [], requests = [];
  let respond = () => status();
  const ctx = vm.createContext({ document: { createElement: tag => new Node(tag) }, setTimeout: fn => { timers.push(fn); return timers.length; }, clearTimeout: () => {} });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../ui/calendar.js'), 'utf8'), ctx);
  const api = async (url, options) => {
    requests.push({ url, options, body: options?.body ? JSON.parse(options.body) : undefined });
    const answer = respond(url, options);
    if (answer instanceof Error) throw answer;
    return answer;
  };
  const all = (n = body) => [n, ...n.children.flatMap(c => all(c))];
  const byId = id => all().find(n => n.id === id);
  return {
    body, timers, requests, respond: fn => { respond = fn; },
    mount: () => ctx.FOTWCalendar.settings(body, api),
    all, byId,
    text: () => all().map(n => n.textContent).join(' | '),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
function status(extra = {}) {
  return { apple_access: 'granted', composio_key_saved: false, composio_acknowledged: false, google_accounts: [], error: null, ...extra };
}

test('a server without the route keeps the section hidden', async () => {
  const w = rig();
  const missing = Object.assign(new Error('nf'), { status: 404 });
  w.respond(() => missing);
  const root = w.mount();
  await flush();
  assert.equal(root.hidden, true);
});

test('shows Apple Calendar access and asks for a key first', async () => {
  const w = rig();
  const root = w.mount();
  await flush();
  assert.equal(root.hidden, false);
  assert.match(w.byId('calendar-apple').textContent, /granted/);
  assert.equal(w.byId('calendar-key-row').hidden, false);
  assert.equal(w.byId('calendar-connect').disabled, true, 'no connect without a key');
});

test('saving a key posts it once and clears the field', async () => {
  const w = rig();
  w.mount();
  await flush();
  w.respond(() => status({ composio_key_saved: true }));
  w.byId('calendar-key').value = 'ck-secret';
  await w.byId('calendar-key-save').click();
  await flush();
  const post = w.requests.find(r => r.body?.action === 'save_key');
  assert.equal(post.body.key, 'ck-secret');
  assert.equal(w.byId('calendar-key').value, '', 'the key does not linger in the page');
  assert.equal(w.byId('calendar-key-row').hidden, true);
  assert.match(w.byId('calendar-key-state').textContent, /saved/i);
});

test('a refused key shows the reason', async () => {
  const w = rig();
  w.mount();
  await flush();
  w.respond(() => status({ error: 'Composio rejected the API key. Check it in Settings.' }));
  w.byId('calendar-key').value = 'nope';
  await w.byId('calendar-key-save').click();
  await flush();
  assert.match(w.byId('calendar-status').textContent, /rejected/);
});

test('connect needs the disclosure ticked, then offers the sign-in link and polls', async () => {
  const w = rig();
  w.respond(() => status({ composio_key_saved: true }));
  w.mount();
  await flush();
  const connect = w.byId('calendar-connect');
  assert.equal(connect.disabled, true, 'disclosure not yet ticked');
  w.byId('calendar-ack').checked = true;
  await w.byId('calendar-ack').change();
  assert.equal(connect.disabled, false);
  w.respond(() => status({
    composio_key_saved: true, composio_acknowledged: true,
    google_accounts: [{ id: 'ca_1', status: 'pending', added_at_ms: 1 }],
    redirect_url: 'https://connect.composio.dev/link/abc',
  }));
  await connect.click();
  await flush();
  const post = w.requests.find(r => r.body?.action === 'connect');
  assert.equal(post.body.acknowledge_egress, true);
  const link = w.byId('calendar-signin');
  assert.equal(link.hidden, false);
  assert.equal(link.href, 'https://connect.composio.dev/link/abc');
  assert.equal(link.target, '_blank');
  assert.equal(link.rel, 'noopener noreferrer');
  assert.match(w.text(), /Waiting for Google sign-in/);
  assert.ok(w.timers.length >= 1, 'polls while an account is pending');

  w.respond(() => status({
    composio_key_saved: true, composio_acknowledged: true,
    google_accounts: [{ id: 'ca_1', status: 'active', added_at_ms: 1 }],
  }));
  await w.timers.pop()();
  await flush();
  assert.equal(w.requests.at(-1).body.action, 'refresh');
  assert.match(w.text(), /Connected/);
  assert.equal(w.byId('calendar-signin').hidden, true);
});

test('a link that is not https is never shown', async () => {
  const w = rig();
  w.respond(() => status({ composio_key_saved: true, composio_acknowledged: true }));
  w.mount();
  await flush();
  w.respond(() => status({ composio_key_saved: true, composio_acknowledged: true, redirect_url: 'javascript:alert(1)' }));
  await w.byId('calendar-connect').click();
  await flush();
  assert.equal(w.byId('calendar-signin').hidden, true);
});

test('disconnect asks for that account only', async () => {
  const w = rig();
  w.respond(() => status({
    composio_key_saved: true, composio_acknowledged: true,
    google_accounts: [{ id: 'ca_1', status: 'active', added_at_ms: 1 }, { id: 'ca_2', status: 'failed', added_at_ms: 2 }],
  }));
  w.mount();
  await flush();
  const buttons = w.all().filter(n => n.tagName === 'button' && n.textContent === 'Disconnect');
  assert.equal(buttons.length, 2);
  assert.match(w.text(), /Sign-in failed/);
  await buttons[1].click();
  await flush();
  assert.deepEqual(w.requests.at(-1).body, { action: 'disconnect', id: 'ca_2' });
});

test('the module never uses innerHTML', () => {
  const source = fs.readFileSync(path.join(__dirname, '../../ui/calendar.js'), 'utf8');
  assert.ok(!/innerHTML|outerHTML|insertAdjacentHTML/.test(source));
});

test('a failed refresh keeps polling', async () => {
  const w = rig();
  w.respond(() => status({ composio_key_saved: true, composio_acknowledged: true, google_accounts: [{ id: 'ca_1', status: 'pending', added_at_ms: 1 }] }));
  w.mount();
  await flush();
  assert.equal(w.timers.length, 1);
  w.respond(() => new Error('network'));
  await w.timers.pop()();
  await flush();
  assert.equal(w.timers.length, 1, 'polling resumes after a failed refresh');
});

test('a failed first load stays visible with a retry', async () => {
  const w = rig();
  w.respond(() => Object.assign(new Error('boom'), { status: 500 }));
  const root = w.mount();
  await flush();
  assert.equal(root.hidden, false);
  assert.match(w.byId('calendar-status').textContent, /Could not load/);
  const retry = w.byId('calendar-retry');
  assert.equal(retry.hidden, false);
  w.respond(() => status());
  await retry.click();
  await flush();
  assert.equal(retry.hidden, true);
  assert.match(w.byId('calendar-apple').textContent, /granted/);
});

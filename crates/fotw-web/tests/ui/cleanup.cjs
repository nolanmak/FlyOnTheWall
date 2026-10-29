const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function rig() {
  class Node {
    constructor(tag) { this.tagName = tag; this.children = []; this.listeners = {}; this.hidden = false; this.disabled = false; this.textContent = ''; }
    appendChild(n) { n.parentNode = this; this.children.push(n); return n; }
    get isConnected() { return this === body || !!this.parentNode?.isConnected; }
    setAttribute(k, v) { this[k] = v; }
    addEventListener(k, fn) { this.listeners[k] = fn; }
    async click() { if (!this.disabled) return this.listeners.click?.(); }
  }
  const body = new Node('body'), timers = [], requests = [];
  let respond = () => ({ phase: 'none', percent: 0, showing: null, detail: null }), reloads = 0;
  const ctx = vm.createContext({ document: { createElement: tag => new Node(tag) }, setTimeout: fn => { timers.push(fn); } });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../ui/cleanup.js'), 'utf8'), ctx);
  const api = async (url, options) => { requests.push({ url, options }); return respond(url, options); };
  const all = (n = body) => [n, ...n.children.flatMap(c => all(c))];
  return {
    body, timers, requests, respond: fn => { respond = fn; },
    mount: id => ctx.FOTWCleanup.mount(id, body, api, () => { reloads++; }),
    settings: () => ctx.FOTWCleanup.settings(body, api),
    all, reloads: () => reloads,
    status: () => all().find(n => n.role === 'status'),
    button: () => all().find(n => n.tagName === 'button' && /transcript$/.test(n.textContent)),
    labelled: label => all().find(n => n.tagName === 'button' && n.textContent === label),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
const state = (phase, extra = {}) => ({ phase, percent: 0, showing: null, detail: null, ...extra });

test('a meeting without cleanup shows nothing and does not poll', async () => {
  const w = rig();
  const root = w.mount('m');
  await flush();
  assert.equal(root.hidden, true);
  assert.equal(w.timers.length, 0);
});

test('a running cleanup shows its percentage, polls, and reloads the transcript once it lands', async () => {
  const w = rig();
  w.respond(() => state('pending', { percent: 40, showing: 'live' }));
  const root = w.mount('m');
  await flush();
  assert.equal(root.hidden, false);
  assert.equal(w.status().textContent, 'Cleaning up transcript… 40%');
  assert.equal(w.button().hidden, true, 'no switch until there is a cleaned version');
  assert.equal(w.timers.length, 1);
  w.respond(() => state('done', { percent: 100, showing: 'cleaned' }));
  w.timers.shift()();
  await flush(); await flush();
  assert.match(w.status().textContent, /^Cleaned transcript/);
  assert.equal(w.reloads(), 1);
  assert.equal(w.button().hidden, false);
  assert.equal(w.button().textContent, 'Show live transcript');
});

test('show live transcript asks for the live version and reloads; the button then offers the cleaned one', async () => {
  const w = rig();
  w.respond(() => state('done', { showing: 'cleaned', detail: 'speaker labels unavailable: no diarizer installed' }));
  w.mount('m 1');
  await flush();
  assert.ok(w.all().some(n => n.textContent === 'speaker labels unavailable: no diarizer installed' && !n.hidden));
  w.respond((url, options) => {
    assert.equal(url, '/api/meetings/m%201/cleanup/show');
    assert.deepEqual(JSON.parse(options.body), { version: 'live' });
    return state('done', { showing: 'live' });
  });
  await w.button().click();
  await flush();
  assert.equal(w.reloads(), 1);
  assert.equal(w.button().textContent, 'Show cleaned transcript');
  assert.match(w.status().textContent, /live transcript/);
});

test('a failed cleanup says the live transcript is shown', async () => {
  const w = rig();
  w.respond(() => state('failed', { showing: 'live', detail: 'Parakeet Unified is not installed' }));
  w.mount('m');
  await flush();
  assert.equal(w.status().textContent, 'Transcript cleanup did not finish. Showing the live transcript.');
  assert.equal(w.button().hidden, true);
  assert.equal(w.timers.length, 0);
});

test('the setting loads and saves the chosen mode', async () => {
  const w = rig();
  w.respond(() => ({ mode: 'auto' }));
  const root = w.settings();
  await flush();
  assert.equal(root.hidden, false);
  const select = w.all().find(n => n.tagName === 'select');
  assert.equal(select.value, 'auto');
  w.respond((url, options) => { assert.equal(url, '/api/settings/cleanup'); return JSON.parse(options.body); });
  select.value = 'on';
  await select.listeners.change();
  assert.equal(select.value, 'on');
  assert.match(w.status().textContent, /^Saved/);
});

test('a running cleanup can be cancelled, keeping the live transcript', async () => {
  const w = rig();
  w.respond(() => state('pending', { percent: 20, showing: 'live' }));
  w.mount('m');
  await flush();
  const cancel = w.labelled('Cancel cleanup');
  assert.equal(cancel.hidden, false);
  assert.equal(w.labelled('Retry cleanup').hidden, true);
  w.respond((url, options) => {
    assert.equal(url, '/api/meetings/m/cleanup/cancel');
    assert.equal(options.method, 'POST');
    assert.equal(options.body, '{}');
    return state('failed', { showing: 'live', detail: 'Cancelled; the live transcript is kept.' });
  });
  await cancel.click();
  await flush();
  assert.equal(w.status().textContent, 'Transcript cleanup cancelled. Showing the live transcript.');
  assert.equal(w.labelled('Cancel cleanup').hidden, true);
  assert.equal(w.labelled('Retry cleanup').hidden, false);
});

test('a failed cleanup offers retry, which polls again', async () => {
  const w = rig();
  w.respond(() => state('failed', { showing: 'live', detail: 'the speech helper crashed' }));
  w.mount('m');
  await flush();
  assert.equal(w.labelled('Cancel cleanup').hidden, true);
  w.respond((url) => {
    assert.equal(url, '/api/meetings/m/cleanup/retry');
    return state('pending', { percent: 0, showing: 'live' });
  });
  const before = w.timers.length;
  await w.labelled('Retry cleanup').click();
  await flush();
  assert.equal(w.status().textContent, 'Cleaning up transcript… 0%');
  assert.equal(w.timers.length, before + 1, 'polls again');
  assert.equal(w.labelled('Retry cleanup').hidden, true);
});

test('a cleaned version that arrived after the summary is offered, not shown', async () => {
  const w = rig();
  w.respond(() => state('done', { showing: 'live' }));
  w.mount('m');
  await flush();
  assert.equal(w.status().textContent, 'Cleaned transcript available. Showing the live transcript.');
  assert.equal(w.button().textContent, 'Show cleaned transcript');
});

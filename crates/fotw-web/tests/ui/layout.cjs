// #232: live capture/transcription status folds into one line so the
// transcript and meeting list keep the screen, and reopens on trouble.
const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');

class Element {
  constructor(tag) { this.tagName = tag; this.children = []; this.attributes = {}; this.textContent = ''; this.hidden = false; this.open = false; this.classes = new Set(); }
  get firstChild() { return this.children[0] || null; }
  appendChild(child) { this.children.push(child); child.parent = this; return child; }
  removeChild(child) { this.children = this.children.filter(c => c !== child); return child; }
  remove() { this.parent?.removeChild(this); }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  getAttribute(name) { return this.attributes[name] ?? null; }
  addEventListener() {}
  get classList() { const c = this.classes; return { toggle: (n, on) => (on ? c.add(n) : c.delete(n)), contains: n => c.has(n) }; }
}

function harness() {
  const nodes = new Map();
  const node = id => { if (!nodes.has(id)) { const n = new Element('div'); n.id = id; nodes.set(id, n); } return nodes.get(id); };
  const ctx = vm.createContext({
    document: { getElementById: node, createElement: tag => new Element(tag) },
    window: { location: { search: '', pathname: '/', hash: '' } }, URLSearchParams,
    localStorage: { getItem: () => null, setItem() {} },
  });
  const source = fs.readFileSync(path.join(__dirname, '../../ui/app.js'), 'utf8').replace(/\nmain\(\);\s*$/, '\n');
  vm.runInContext(source, ctx);
  vm.runInContext('recState = "recording"', ctx);
  return { ctx, node };
}

const status = (system, mic = 'healthy') => ({
  session_id: 's1',
  capture_health: { system: { state: system }, mic: { state: mic } },
  speech_status: [
    { source: 'system', provider: 'deepgram', model: 'nova-3', state: 'open' },
    { source: 'mic', provider: 'deepgram', model: 'nova-3', state: 'open' },
  ],
});
const render = (h, body) => { h.ctx.renderCaptureHealth(body); h.ctx.renderSpeechHealth(body); };

test('a healthy recording shows one collapsed status line', () => {
  const h = harness();
  render(h, status('healthy'));
  const box = h.node('live-status');
  assert.equal(box.hidden, false);
  assert.equal(box.open, false, 'healthy status must not take over the screen');
  const line = h.node('live-status-summary').textContent;
  assert.match(line, /System audio: Healthy/);
  assert.match(line, /Microphone: Healthy/);
  assert.match(line, /nova-3/);
});

test('a source needing attention opens the panel once and respects a later collapse', () => {
  const h = harness();
  render(h, status('healthy'));
  render(h, status('exhausted'));
  const box = h.node('live-status');
  assert.equal(box.open, true, 'the Retry button must not hide behind a collapsed line');
  assert.equal(box.classList.contains('attention'), true);
  box.open = false; // the user collapses it
  render(h, status('exhausted'));
  assert.equal(box.open, false);
  render(h, status('healthy'));
  assert.equal(box.classList.contains('attention'), false);
  render(h, status('recovering'));
  assert.equal(box.open, true, 'a new problem reopens it');
});

test('the status bar disappears when nothing is recording', () => {
  const h = harness();
  render(h, status('healthy'));
  vm.runInContext('recState = "idle"', h.ctx);
  render(h, {});
  assert.equal(h.node('live-status').hidden, true);
});

const speechState = (body, source, state) => ({
  ...body,
  speech_status: body.speech_status.map(s => (s.source === source ? { ...s, state } : s)),
});

test('a recording that ended while expanded leaves the next one collapsed', () => {
  const h = harness();
  render(h, status('exhausted'));
  assert.equal(h.node('live-status').open, true);
  vm.runInContext('recState = "idle"', h.ctx);
  render(h, {});
  vm.runInContext('recState = "recording"', h.ctx);
  render(h, { ...status('healthy'), session_id: 's2' });
  assert.equal(h.node('live-status').open, false);
});

test('a stopped transcription is named in the collapsed line even while capture is healthy', () => {
  const h = harness();
  render(h, speechState(status('healthy'), 'system', 'closed'));
  assert.match(h.node('live-status-summary').textContent, /System audio: Healthy[^|]*Stream closed/);
});

test('a second source needing attention reopens a collapsed bar', () => {
  const h = harness();
  render(h, status('exhausted'));
  const box = h.node('live-status');
  box.open = false; // user collapses after seeing the system problem
  render(h, status('exhausted', 'recovering'));
  assert.equal(box.open, true, 'the microphone problem is new and must be shown');
  box.open = false;
  render(h, status('exhausted', 'recovering'));
  assert.equal(box.open, false, 'repeating the same problems stays collapsed');
});

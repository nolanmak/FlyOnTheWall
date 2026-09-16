// The per-meeting GitHub sync line (issue #112), driven through a DOM.
//
// What this file exists to prove is mostly a *negative*: no meeting offers a
// push button any more, in any state, because the worker owns every push and a
// button on a synced meeting invited a second commit of a meeting already in
// the repository. The positive half is that each state says one true thing, and
// that exactly one of them — failed — offers a control.
//
// The same shape as documents.cjs: a small hand-rolled DOM, because the point
// is which nodes get built and what they say, and neither jsdom nor a browser
// is needed to answer that.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

// `text`, `clear`, `say` and `api` are app.js's, and the module under test
// takes them from the global scope exactly as documents.js does.
function harness() {
  class Element {
    constructor(tag) {
      this.tagName = tag;
      this.children = [];
      this.listeners = {};
      this.attributes = {};
    }
    get firstChild() { return this.children[0]; }
    get textContent() { return (this.ownText || '') + this.children.map(c => c.textContent).join(''); }
    set textContent(v) { this.ownText = String(v); this.children = []; }
    appendChild(c) { c.parent = this; this.children.push(c); return c; }
    append(...kids) { kids.forEach(c => this.appendChild(c)); }
    removeChild(c) { this.children.splice(this.children.indexOf(c), 1); c.parent = null; }
    remove() { if (this.parent) this.parent.removeChild(this); }
    addEventListener(name, fn) { this.listeners[name] = fn; }
    setAttribute(name, value) { this.attributes[name] = value; }
    click() { if (!this.disabled) return this.listeners.click?.(); }
  }
  const body = new Element('body');
  const requests = [];
  const said = [];
  // What `GET /api/meetings/{id}/github-sync` answers, per test.
  let status = { state: 'off' };
  // What a retry's POST answers.
  let pushResult = { receipt: { repo: 'octocat/notes', path: 'meetings/x.md' } };
  const ctx = vm.createContext({
    Date, Set, Map, Intl,
    document: {
      body,
      createElement: tag => new Element(tag),
      createTextNode: v => { const e = new Element('#text'); e.textContent = v; return e; },
    },
    text: (tag, value, cls) => { const e = new Element(tag); e.textContent = value; e.className = cls; return e; },
    clear: el => { while (el.firstChild) el.removeChild(el.firstChild); },
    say: message => { said.push(message); },
    api: async (requested, opts) => {
      const method = (opts && opts.method) || 'GET';
      requests.push({ path: requested, method });
      return method === 'POST' ? pushResult : status;
    },
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../ui/github.js'), 'utf8'), ctx);
  const all = (node = body) => [node, ...node.children.flatMap(c => all(c))];
  return {
    requests, said,
    status: value => { status = value; },
    pushResult: value => { pushResult = value; },
    // A meeting is `ready` unless a test says otherwise: that is the only
    // state an export has anything to say about.
    mount: (meeting = { id: 'm1', state: 'ready' }) => {
      const host = body.appendChild(new Element('div'));
      ctx.mountGithubSync({ meeting }, host);
      return host;
    },
    buttons: () => all().filter(n => n.tagName === 'button'),
    lines: () => all().filter(n => n.className === 'gh-sync').map(n => n.textContent),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));

// 2026-08-22 00:16 EDT, the auto stamp on the library that reported the issue.
const PUSHED_AT = Date.parse('2026-08-22T04:16:36Z');

// A1. The whole point of the issue: the dashboard has no always-present push
// button. Every state is checked, because the button used to be drawn from the
// settings alone and so appeared in all of them.
test('no meeting state offers a push button', async () => {
  for (const state of [
    { state: 'off' },
    { state: 'never' },
    { state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT },
    { state: 'changed', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT },
  ]) {
    const w = harness();
    w.status(state);
    w.mount();
    await flush();
    assert.deepEqual(
      w.buttons().map(b => b.textContent), [],
      'state ' + state.state + ' must offer nothing to click',
    );
  }
});

// A2. Exactly one line, and it says which of the four states this is.
test('every state renders exactly one sync line, and says which state it is', async () => {
  const cases = [
    [{ state: 'never' }, /Not synced to GitHub yet/],
    [{ state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT }, /Synced to octocat\/notes/],
    [{ state: 'changed', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT }, /Changed since/],
    [{ state: 'failed', repo: 'octocat/notes', error: 'HTTP 422' }, /did not sync/],
  ];
  for (const [status, expected] of cases) {
    const w = harness();
    w.status(status);
    w.mount();
    await flush();
    const lines = w.lines();
    assert.equal(lines.length, 1, 'state ' + status.state + ' rendered ' + lines.length + ' line(s)');
    assert.match(lines[0], expected);
  }
});

// A synced meeting says *when*, which is the fact the old button could not
// carry: "is this already in the repository" was unanswerable from the screen.
test('a synced meeting names the repository and when it was synced', async () => {
  const w = harness();
  w.status({ state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT });
  w.mount();
  await flush();
  const line = w.lines()[0];
  assert.match(line, /octocat\/notes/);
  assert.match(line, new RegExp(String(new Date(PUSHED_AT).getFullYear())));
});

// `off` is the read-only build and the switched-off target: the section is
// absent rather than showing a state about a feature that is not running.
test('export switched off renders nothing at all', async () => {
  const w = harness();
  w.status({ state: 'off' });
  const host = w.mount();
  await flush();
  assert.equal(host.children.length, 0);
  assert.deepEqual(w.lines(), []);
});

// A3. The one control, on the one state that can use it.
test('only a failed meeting offers Retry, and it re-attempts immediately', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'octocat/notes', error: 'HTTP 422' });
  w.mount();
  await flush();
  const buttons = w.buttons();
  assert.deepEqual(buttons.map(b => b.textContent), ['Retry']);

  // The retry is a push of this meeting, now — not a note asking the worker to
  // try again on some later pass.
  w.status({ state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT });
  await buttons[0].click();
  await flush();
  assert.deepEqual(
    w.requests.filter(r => r.method === 'POST').map(r => r.path),
    ['/api/meetings/m1/github-push'],
  );
  assert.deepEqual(w.buttons(), [], 'a retry that worked leaves nothing to click');
  assert.match(w.lines()[0], /Synced to octocat\/notes/);
});

// The failure reason is `gh`'s own stderr about a repository. It has to be on
// screen — a meeting that silently did not sync is the bug this replaces — and
// it is provider text, so it arrives as a text node.
test('a failed meeting shows the reason the daemon reported', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'octocat/notes', error: 'gh: Validation Failed (HTTP 422)' });
  w.mount();
  await flush();
  assert.match(w.lines()[0], /gh: Validation Failed \(HTTP 422\)/);
});

// A failed retry says so and keeps the control, rather than clearing the state
// and leaving the meeting looking fine.
test('a retry that fails again keeps the control and says what happened', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'octocat/notes', error: 'HTTP 422' });
  w.mount();
  await flush();
  w.pushResult({ error: 'gh_not_authenticated' });
  await w.buttons()[0].click();
  await flush();
  assert.deepEqual(w.buttons().map(b => b.textContent), ['Retry']);
  assert.ok(
    w.said.some(m => /gh auth login/.test(m)),
    'the machine code is explained rather than shown raw: ' + w.said.join(' | '),
  );
});

// The retry is automatic too, so the line says when the worker will try on its
// own. Otherwise the control reads as the only way back, which is what the old
// button wrongly implied.
test('a failed meeting says the worker will retry by itself', async () => {
  const w = harness();
  w.status({
    state: 'failed', repo: 'octocat/notes', error: 'HTTP 422',
    retry_at_ms: PUSHED_AT + 5 * 60 * 1000,
  });
  w.mount();
  await flush();
  assert.match(w.lines()[0], /automatically|on its own|retr/i);
});

// A meeting still recording has no export state worth reporting, and asking
// about one would be a request for a meeting that cannot have been pushed.
test('a meeting that is not ready is not asked about at all', async () => {
  const w = harness();
  const host = w.mount({ id: 'm1', state: 'recording' });
  await flush();
  assert.deepEqual(w.requests, []);
  assert.equal(host.children.length, 0);
});

// The per-meeting GitHub sync line (issue #112), driven through a DOM.
//
// What this file exists to prove is mostly a *negative*: no meeting offers a
// push button any more, in any state, because the worker owns every push and a
// button on a synced meeting invited a second commit of a meeting already in
// the repository. The positive half is that each state says one true thing, and
// that a control appears exactly where the worker will *not* do the job — so a
// reader can tell "nothing to do, it is coming" from "nothing happens unless
// you click".
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
    // The pane is reused for every meeting, so a node's own answer to "am I
    // still on screen" is what a load that arrived late has to consult.
    get isConnected() { return this === body || Boolean(this.parent?.isConnected); }
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
  // What `GET /api/meetings/{id}/github-sync` answers, per test. A function may
  // stand in for the value, so a test can hold a response open or throw.
  let status = { state: 'off' };
  // What a push answers, under the same rule.
  let pushResult = { receipt: { repo: 'octocat/notes', path: 'meetings/x.md' } };
  const answer = (value, requested) => (typeof value === 'function' ? value(requested) : value);
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
      return method === 'POST' ? answer(pushResult, requested) : answer(status, requested);
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
    // The pane `renderDetail` reuses for every meeting, and what it does to it
    // before drawing the next one.
    host: () => body.appendChild(new Element('div')),
    mountInto: (host, meeting) => ctx.mountGithubSync({ meeting }, host),
    clearHost: host => { while (host.firstChild) host.removeChild(host.firstChild); },
    buttons: () => all().filter(n => n.tagName === 'button'),
    panels: () => all().filter(n => n.className === 'gh-sync-panel'),
    lines: () => all().filter(n => n.className === 'gh-sync').map(n => n.textContent),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));

// 2026-08-22 00:16 EDT, the auto stamp on the library that reported the issue.
const PUSHED_AT = Date.parse('2026-08-22T04:16:36Z');

// A1. The whole point of the issue: the dashboard has no always-present push
// button. Every state is checked, because the button used to be drawn from the
// settings alone and so appeared in all of them.
test('no meeting the worker will sync offers a push button', async () => {
  for (const state of [
    { state: 'off' },
    { state: 'never', scheduled: true },
    { state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT, scheduled: true },
    { state: 'changed', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT, scheduled: true },
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

// ------------------------------------------------- the scope of a pass (#112)

// F3. A meeting no automatic pass will reach is the one case where a person has
// to be able to act: manual mode, or a meeting recorded before automatic pushes
// were switched on. The control exists exactly there, and the line says plainly
// that nothing is coming — the difference the pane could not express while
// every state read the same.
test('a meeting no pass will sync offers Sync now and says nothing is coming', async () => {
  const w = harness();
  w.status({ state: 'never', scheduled: false });
  w.mount();
  await flush();
  assert.deepEqual(w.buttons().map(b => b.textContent), ['Sync now']);
  assert.match(w.lines()[0], /No automatic pass/);

  w.status({ state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT, scheduled: false });
  await w.buttons()[0].click();
  await flush();
  assert.deepEqual(
    w.requests.filter(r => r.method === 'POST').map(r => r.path),
    ['/api/meetings/m1/github-push'],
    'Sync now is a push of this meeting, now',
  );
  assert.deepEqual(w.buttons(), [], 'a meeting that is now in the repository offers nothing');
});

// The other half of the same distinction: a meeting the worker will take needs
// no control, and says that it is coming rather than saying nothing.
test('a meeting a pass will sync offers nothing and says it is coming', async () => {
  const w = harness();
  w.status({ state: 'never', scheduled: true });
  w.mount();
  await flush();
  assert.deepEqual(w.buttons(), [], 'the worker owns this push; a control would duplicate it');
  assert.match(w.lines()[0], /next pass/);
});

test('a changed meeting outside every pass offers Sync now', async () => {
  const w = harness();
  w.status({ state: 'changed', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT, scheduled: false });
  w.mount();
  await flush();
  assert.deepEqual(w.buttons().map(b => b.textContent), ['Sync now']);
  assert.match(w.lines()[0], /No automatic pass/);
  assert.doesNotMatch(w.lines()[0], /next pass will sync it again/);
});

// F2. The promise of an automatic retry belongs to the daemon, and it makes it
// by sending a retry time. Without one, nothing is going to happen on its own,
// and the line must not suggest otherwise.
test('a failure nobody will retry says so, and still offers Retry', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'octocat/notes', error: 'HTTP 422', scheduled: false });
  w.mount();
  await flush();
  assert.deepEqual(w.buttons().map(b => b.textContent), ['Retry']);
  assert.match(w.lines()[0], /not be retried/i);
  assert.doesNotMatch(w.lines()[0], /will try again on its own/);
});

// A meeting that synced before and failed on a later change is not a meeting
// that was never synced, and the older copy in the repository is still there.
test('a meeting that synced before and failed later says when it last synced', async () => {
  const w = harness();
  w.status({
    state: 'failed', repo: 'octocat/notes', error: 'HTTP 422',
    pushed_at_ms: PUSHED_AT, retry_at_ms: PUSHED_AT + 5 * 60 * 1000,
  });
  w.mount();
  await flush();
  const line = w.lines()[0];
  assert.match(line, /HTTP 422/);
  assert.match(line, /last synced/i);
  assert.match(line, new RegExp(String(new Date(PUSHED_AT).getFullYear())));
});

// F4. The refusals that answer for every meeting — no gh, no login, the
// repository gone or public — are not this meeting's fault, and nothing retries
// them on a schedule. The pane says what is wrong instead of "not synced to
// GitHub yet" forever.
test('a meeting blocked by the environment names the reason and promises no retry', async () => {
  const w = harness();
  w.status({
    state: 'never', scheduled: true,
    blocked: 'gh_not_authenticated', blocked_at_ms: PUSHED_AT,
  });
  w.mount();
  await flush();
  const line = w.lines()[0];
  assert.match(line, /gh auth login/, 'the machine code is explained, as a push failure is');
  assert.doesNotMatch(line, /will try again on its own/);
  assert.deepEqual(w.buttons(), [], 'nothing a click could do while gh has no login');
});

// ------------------------------------------------ the pane it lives in (#112)

// F5. `#detail` is one node, reused for every meeting, and `renderDetail`
// clears it. The panel is owned before the first await, so a response that
// arrives after the reader moved on has nothing on screen to append to. Two
// overlapping loads used to leave two sync lines in the pane.
test('switching meetings mid-load leaves exactly one sync line', async () => {
  const w = harness();
  const pending = [];
  w.status(() => new Promise(resolve => { pending.push(resolve); }));
  const host = w.host();
  w.mountInto(host, { id: 'm1', state: 'ready' });
  // What renderDetail does before drawing the next meeting: same node, emptied.
  w.clearHost(host);
  w.mountInto(host, { id: 'm2', state: 'ready' });

  pending[0]({ state: 'never', scheduled: true });
  pending[1]({ state: 'synced', repo: 'octocat/notes', pushed_at_ms: PUSHED_AT, scheduled: true });
  await flush();
  await flush();

  assert.equal(w.lines().length, 1, 'the abandoned load must not add a second line');
  assert.match(w.lines()[0], /Synced to octocat\/notes/, 'and the one left is the meeting on screen');
});

// A control whose push is in flight must not take a second click: that is the
// double commit the claim in the daemon exists to refuse, arriving from the one
// place that can avoid making the request at all.
test('the control is disabled while its push is in flight', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'octocat/notes', error: 'HTTP 422', retry_at_ms: PUSHED_AT });
  w.mount();
  await flush();
  let finish;
  w.pushResult(() => new Promise(resolve => { finish = resolve; }));
  const retrying = w.buttons()[0].click();
  assert.equal(w.buttons()[0].disabled, true, 'a second click would push the same meeting twice');
  finish({ receipt: { repo: 'octocat/notes', path: 'meetings/x.md' } });
  await retrying;
  await flush();
  assert.equal(
    w.requests.filter(r => r.method === 'POST').length, 1,
    'one click, one push',
  );
});

// The repository a retry is aimed at is the one the failure is about, and the
// state carries it. Reading it from the settings form instead names whatever is
// configured right now, which is not necessarily the same repository.
test('a retry names the repository the failure is about', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'work-org/minutes', error: 'HTTP 422' });
  w.mount();
  await flush();
  await w.buttons()[0].click();
  await flush();
  assert.ok(
    w.said.some(m => /work-org\/minutes/.test(m)),
    'the state names the repository the push failed against: ' + w.said.join(' | '),
  );
});

// A 404 is this build having no GitHub export at all — the convention the
// recorder and the settings form use. Anything else is a problem worth saying:
// a 500, an expired token or a daemon restart must not render as "there is no
// such feature" and take the meeting's state off screen.
test('a 404 removes the panel and any other failure reports a problem', async () => {
  const absent = harness();
  absent.status(() => { const e = new Error('request failed'); e.status = 404; throw e; });
  const host = absent.mount();
  await flush();
  assert.equal(host.children.length, 0, 'no GitHub export, no section');

  const broken = harness();
  broken.status(() => { const e = new Error('request failed'); e.status = 500; throw e; });
  broken.mount();
  await flush();
  assert.equal(broken.lines().length, 1);
  assert.match(broken.lines()[0], /could not be read|Could not/i);
});

// Out of context "Retry" says nothing about what is being retried, and an
// unnamed <section> is one more anonymous group in a pane that already has
// several.
test('the panel and its control have accessible names that say what they are', async () => {
  const w = harness();
  w.status({ state: 'failed', repo: 'octocat/notes', error: 'HTTP 422' });
  w.mount();
  await flush();
  assert.match(w.panels()[0].attributes['aria-label'] || '', /GitHub/);
  const label = w.buttons()[0].attributes['aria-label'] || '';
  assert.match(label, /GitHub/, 'the control names what it acts on: ' + label);
  assert.notEqual(label, 'Retry');
});

// WCAG 2.5.3 Label in Name: the accessible name has to *contain* the visible
// label, or a speech-input user saying "click Sync now" cannot activate the one
// control an out-of-scope meeting has.
test('every control accessible name contains its visible label', async () => {
  for (const status of [
    { state: 'failed', repo: 'octocat/notes', error: 'HTTP 422' },
    { state: 'never', scheduled: false },
  ]) {
    const w = harness();
    w.status(status);
    w.mount();
    await flush();
    const button = w.buttons()[0];
    const label = button.attributes['aria-label'] || '';
    assert.ok(
      label.includes(button.textContent),
      'state ' + status.state + ': "' + label + '" must contain "' + button.textContent + '"',
    );
    assert.notEqual(label, button.textContent, 'and say more than the label alone');
    assert.match(label, /GitHub/);
  }
});

// A broken environment on a meeting no pass covers: the person looking at it is
// the only one who can push it, so the reason has to be on screen *and* the
// control has to stay.
test('a blocked meeting outside every pass names the reason and keeps Sync now', async () => {
  const w = harness();
  w.status({ state: 'never', scheduled: false, blocked: 'gh_not_authenticated', blocked_at_ms: PUSHED_AT });
  w.mount();
  await flush();
  assert.match(w.lines()[0], /gh auth login/);
  assert.deepEqual(w.buttons().map(b => b.textContent), ['Sync now']);
});

// The blocked line composes the code's explanation into its own sentence, so an
// explanation that opens with its own "Nothing was pushed" read as two verdicts
// stapled together.
test('the blocked line reads as one sentence for every code', async () => {
  for (const code of ['gh_missing', 'gh_not_authenticated', 'repo_not_found', 'repo_is_public', 'github_export_disabled']) {
    const w = harness();
    w.status({ state: 'never', scheduled: true, blocked: code });
    w.mount();
    await flush();
    const line = w.lines()[0];
    assert.doesNotMatch(line, /Nothing was pushed/, code + ': ' + line);
    assert.doesNotMatch(line, /:\s*[A-Z][a-z]+ was/, code + ' reads as two verdicts: ' + line);
  }
});

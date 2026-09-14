// No dependencies: exercise export privacy boundaries and Eastern clock math.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const context = vm.createContext({ Intl, Date, Set, Map });
vm.runInContext(fs.readFileSync(path.join(__dirname, '../../ui/documents.js'), 'utf8'), context);
function editor() {
  return { markdown: '# Project recap\n\nSend the draft.', includeTranscript: true, excluded: new Set(),
    document: { title: 'Project recap', started_at_ms: Date.parse('2026-07-14T14:05:12Z'), source_segments: 5,
      excerpts: [{index:0,offset_ms:0,at_ms:Date.parse('2026-07-14T14:05:12Z'),speaker:'Call audio',text:'Send the draft.'},
        {index:2,offset_ms:1512000,at_ms:Date.parse('2026-07-14T14:30:24Z'),speaker:'Call audio',text:'Thanks, goodbye.'}] } };
}
test('Eastern time uses meeting date, DST and the correct calendar day', () => {
  assert.match(context.sharingTime(Date.parse('2026-07-14T14:05:12Z')), /10:05:12 AM EDT/);
  assert.match(context.sharingTime(Date.parse('2026-01-14T14:05:12Z')), /9:05:12 AM EST/);
  assert.match(context.sharingTime(Date.parse('2026-07-15T02:00:00Z')), /Jul 14, 2026.*10:00:00 PM EDT/);
  assert.match(context.sharingTime(Date.parse('2026-11-01T05:30:00Z')), /1:30:00 AM EDT/);
  assert.match(context.sharingTime(Date.parse('2026-11-01T06:30:00Z')), /1:30:00 AM EST/);
  assert.equal(context.sharingOffset(1512000), '00:25:12');
  assert.equal(context.sharingOffset(3661000), '01:01:01');
});
test('exports only selected source, clearly marks gaps, and honors reviewer removals', () => {
  const e = editor();
  let md = context.sharingMarkdown(e);
  assert.match(md, /00:25:12 \| Jul 14, 2026/);
  assert.match(md, /Conversation omitted/);
  assert.match(md, /Remaining conversation omitted/);
  e.excluded.add(2);
  assert.doesNotMatch(context.sharingMarkdown(e), /goodbye/);
  e.includeTranscript = false;
  assert.doesNotMatch(context.sharingMarkdown(e), /Call audio|Selected meeting transcript/);
  assert.match(context.sharingMarkdown(e), /Send the draft/);
});
test('Markdown source cannot embed active HTML or remote images', () => {
  const e = editor();
  e.document.excerpts[0].text = '<script>alert(1)</script>\n![secret](https://evil.test)';
  const md = context.sharingMarkdown(e);
  assert.doesNotMatch(md, /<script>/);
  assert.doesNotMatch(md, /\n!\[/);
  assert.match(md, /\\<script\\>/);
  assert.equal(context.sharingFilename({title:'../../a/b: résumé?'}), 'ab résumé');
});

// Small DOM harness for the asynchronous create/save/download workflow.
function workflow() {
  class Element {
    constructor(tag) { this.tagName = tag; this.children = []; this.listeners = {}; this.value = ''; }
    get isConnected() { return this === body || Boolean(this.parent?.isConnected); }
    get firstChild() { return this.children[0]; }
    get textContent() { return (this.ownText || '') + this.children.map(c => c.textContent).join(''); }
    set textContent(v) { this.ownText = String(v); this.children = []; }
    appendChild(c) { c.parent = this; this.children.push(c); return c; }
    append(...children) { children.forEach(c => this.appendChild(c)); }
    removeChild(c) { this.children.splice(this.children.indexOf(c), 1); c.parent = null; }
    remove() { if (this.parent) this.parent.removeChild(this); }
    addEventListener(name, fn) { this.listeners[name] = fn; }
    setAttribute() {}
    click() {
      if (this.disabled) return;
      if (this.tagName === 'a') {
        if (failDownload) throw Error('download blocked');
        downloads.push({filename: this.download, blob: blobs.get(this.href)});
      }
      return this.listeners.click?.();
    }
  }
  const body = new Element('body');
  const requests = [], downloads = [], blobs = new Map();
  let failDownload = false;
  let respond = () => ({document: null, revision: 0, automatic: true});
  const ctx = vm.createContext({ Intl, Date, Set, Map, Blob,
    document: {body, createElement: tag => new Element(tag), createTextNode: v => {const e = new Element('#text'); e.textContent = v; return e;}},
    text: (tag, value, cls) => {const e = new Element(tag); e.textContent = value; e.className = cls; return e;},
    clear: el => { while (el.firstChild) el.removeChild(el.firstChild); },
    renderMarkdown: (md, el) => {const e = new Element('p');e.textContent = md;el.appendChild(e);},
    api: async (_, opts) => {const req = JSON.parse(opts.body); requests.push(req); return respond(req);},
    URL: {createObjectURL: blob => {const u = 'blob:' + blobs.size; blobs.set(u,blob);return u;}, revokeObjectURL: () => {}},
    setTimeout: () => {},
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../ui/documents.js'), 'utf8'), ctx);
  const all = (node = body) => [node, ...node.children.flatMap(c => all(c))];
  return {ctx, requests, downloads,
    respond: fn => {respond = fn;}, blockDownload: v => {failDownload = v;},
    mount: id => {const host = body.appendChild(new Element('div'));ctx.mountSharingDocument({meeting:{id,state:'ready'}},host);return host;},
    button: label => all().find(n => n.tagName === 'button' && n.textContent === label),
    find: tag => all().find(n => n.tagName === tag),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
function savedState() {
  const e = editor();
  return {document:{...e.document, markdown:e.markdown, review_notes:[], audience:'Client'}, revision:1, automatic:true};
}
test('create downloads exactly once after generation succeeds, never merely on load', async () => {
  const w = workflow(); w.mount('meeting'); await flush();
  assert.equal(w.downloads.length, 0);
  let finish;
  w.respond(req => req.action === 'generate' ? new Promise(resolve => {finish = resolve;}) : savedState());
  const creating = w.button('Create & download .md').click();
  assert.equal(w.downloads.length, 0, 'no download before the draft exists');
  assert.equal(w.button('Create & download .md').disabled, true);
  finish(savedState()); await creating;
  assert.equal(w.downloads.length, 1);
  assert.equal(w.downloads[0].filename, 'Project recap.md');
  assert.match(await w.downloads[0].blob.text(), /Send the draft/);
  assert.ok(w.button('Download .md'));
  w.mount('meeting'); await flush();
  assert.equal(w.downloads.length, 1, 'reopening a meeting must not download again');
});
test('failed generation downloads nothing; retry uses the same simple button', async () => {
  const w = workflow();w.mount('meeting');await flush();
  w.respond(() => ({error:'Generation failed'}));
  await w.button('Create & download .md').click();
  assert.equal(w.downloads.length, 0);
  assert.equal(w.button('Create & download .md').disabled, false);
});
test('a failed download preserves the draft and retries without another model call', async () => {
  const w = workflow();w.mount('meeting');await flush();
  w.respond(() => savedState());w.blockDownload(true);
  await w.button('Create & download .md').click();
  assert.ok(w.button('Download .md'));
  w.blockDownload(false);w.button('Download .md').click();
  assert.equal(w.downloads.length, 1);
  assert.equal(w.requests.filter(r => r.action === 'generate').length, 1);
});
test('editing uses Save & download and exports the saved edits', async () => {
  const w = workflow();w.respond(() => savedState());w.mount('meeting');await flush();
  const area = w.find('textarea');area.value = '# Updated brief\n\nShip on Monday.';area.listeners.input();
  assert.ok(w.button('Save & download .md'));
  w.respond(req => {const state = savedState();state.revision=2;state.document.markdown=req.markdown;return state;});
  await w.button('Save & download .md').click();
  assert.equal(w.requests.at(-1).action, 'save');
  assert.match(await w.downloads[0].blob.text(), /Ship on Monday/);
});

test('name corrections mark source edits without changing stored excerpts or matching other names', () => {
  const e = editor();
  e.document.name_corrections = [{from:'Marta',to:'Martha'}, {from:'Marta Lopez',to:'Martha Lopez'}];
  e.document.excerpts[0].text = "Marta Lopez's notes. Martas and ÉMarta are unchanged.";
  const md = context.sharingMarkdown(e);
  assert.match(md, /User-confirmed name corrections/);
  assert.match(md, /Martha Lopez/);
  assert.match(md, /Martas and ÉMarta/);
  assert.equal(e.document.excerpts[0].text, "Marta Lopez's notes. Martas and ÉMarta are unchanged.");
});

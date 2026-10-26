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
test('download name follows the live meeting title over the stored document title', () => {
  // The meeting title is the shared name, so two calls with one client that
  // once produced the same generic document name download differently once
  // renamed — even before the draft is regenerated.
  assert.equal(context.sharingFilename({title:'Client recap'}, 'Acme Q3 kickoff'), 'Acme Q3 kickoff');
  // An untitled meeting (blank title) keeps the document's own inferred name.
  assert.equal(context.sharingFilename({title:'Client recap'}, '   '), 'Client recap');
  assert.equal(context.sharingFilename({title:'Client recap'}, ''), 'Client recap');
});

// Small DOM harness for the asynchronous create/save/download workflow.
function workflow(sourcePath = path.join(__dirname, '../../ui/documents.js')) {
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
    api: async (url, opts) => {const req = JSON.parse(opts.body); requests.push(req); return respond(req,url);},
    URL: {createObjectURL: blob => {const u = 'blob:' + blobs.size; blobs.set(u,blob);return u;}, revokeObjectURL: () => {}},
    setTimeout: () => {},
  });
  vm.runInContext(fs.readFileSync(sourcePath, 'utf8'), ctx);
  const all = (node = body) => [node, ...node.children.flatMap(c => all(c))];
  return {ctx, requests, downloads,
    respond: fn => {respond = fn;}, blockDownload: v => {failDownload = v;},
    mount: id => {const host = body.appendChild(new Element('div'));ctx.mountSharingDocument({meeting:{id,state:'ready'}},host);return host;},
    navigate: id => {while(body.firstChild)body.removeChild(body.firstChild);const host=body.appendChild(new Element('div'));ctx.mountSharingDocument({meeting:{id,state:'ready'}},host);return host;},
    button: label => all().find(n => n.tagName === 'button' && n.textContent === label),
    find: tag => all().find(n => n.tagName === tag),
    input: label => all().find(n=>n.tagName==='label'&&n.textContent===label)?.children.find(n=>n.tagName==='input'),
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
test('read-only navigation releases detached document panels to the garbage collector', () => {
  const {spawnSync}=require('node:child_process');
  const source=`
    const fs=require('node:fs'),vm=require('node:vm'),path=require('node:path');
    const workflow=(${workflow.toString()});
    (async()=>{
      const w=workflow(${JSON.stringify(path.join(__dirname,'../../ui/documents.js'))});
      const references=[];
      for(let i=0;i<20;i++){
        let host=w.navigate('meeting-'+i);
        await new Promise(setImmediate);
        references.push(new WeakRef(host));host=null;
      }
      w.navigate('last');await new Promise(setImmediate);
      for(let i=0;i<8;i++){await new Promise(setImmediate);global.gc();}
      const retained=references.filter(r=>r.deref()).length;
      if(retained!==0)throw Error(retained+' detached clean document panels remain reachable');
      process.stdout.write(JSON.stringify({visited:20,retained}));
    })().catch(error=>{console.error(error);process.exitCode=1});
  `;
  const result=spawnSync(process.execPath,['--expose-gc','-e',source],{encoding:'utf8',timeout:15000});
  assert.equal(result.status,0,result.stderr||String(result.error));
  assert.deepEqual(JSON.parse(result.stdout),{visited:20,retained:0});
});
function savedState() {
  const e = editor();
  return {document:{...e.document, markdown:e.markdown, review_notes:[], audience:'Client'}, revision:1, automatic:true};
}
test('retaining an unsaved draft does not retain its detached editor panel',()=>{
 const {spawnSync}=require('node:child_process');
 const source=`
   const fs=require('node:fs'),vm=require('node:vm'),path=require('node:path');
   const workflow=(${workflow.toString()}),editor=(${editor.toString()}),savedState=(${savedState.toString()});
   (async()=>{
     const w=workflow(${JSON.stringify(path.join(__dirname,'../../ui/documents.js'))});
     w.respond(()=>savedState());let host=w.navigate('A');await new Promise(setImmediate);
     let area=w.find('textarea');area.value='Preserve my draft';area.listeners.input();area=null;
     const old=new WeakRef(host);host=null;w.navigate('B');await new Promise(setImmediate);
     for(let i=0;i<8;i++){await new Promise(setImmediate);global.gc();}
     if(old.deref())throw Error('the retained draft still roots its detached panel');
     w.navigate('A');await new Promise(setImmediate);
     if(w.find('textarea').value!=='Preserve my draft')throw Error('collecting the panel lost the draft');
   })().catch(error=>{console.error(error);process.exitCode=1});
 `;
 const result=spawnSync(process.execPath,['--expose-gc','-e',source],{encoding:'utf8',timeout:15000});
 assert.equal(result.status,0,result.stderr||String(result.error));
});
test('unsaved sharing edits survive navigation and save with their original revision',async()=>{
 const w=workflow();w.respond(()=>savedState());w.navigate('A');await flush();
 let area=w.find('textarea');area.value='Unsaved reviewer edits';area.listeners.input();
 w.navigate('B');await flush();w.navigate('A');await flush();
 assert.equal(w.find('textarea').value,'Unsaved reviewer edits');
 assert.equal(w.requests.filter(r=>r.action==='load').length,2,'returning to a dirty draft must not replace it from the server');
 w.respond(req=>({...savedState(),revision:2,document:{...savedState().document,markdown:req.markdown}}));
 await w.button('Save & download .md').click();
 const save=w.requests.find(r=>r.action==='save');
 assert.equal(save.revision,1);assert.equal(save.markdown,'Unsaved reviewer edits');
 assert.match(await w.downloads[0].blob.text(),/Unsaved reviewer edits/);
});
test('local generation preferences survive navigation while clean saved content is refreshed',async()=>{
 const w=workflow();w.respond(()=>savedState());w.navigate('A');await flush();
 const purpose=w.input('Meeting purpose');purpose.value='Follow-up tasks';purpose.listeners.input();
 w.navigate('B');await flush();
 w.respond(()=>({...savedState(),document:{...savedState().document,markdown:'Fresh saved content'}}));
 w.navigate('A');await flush();
 assert.equal(w.input('Meeting purpose').value,'Follow-up tasks');
 assert.equal(w.find('textarea').value,'Fresh saved content');
});
test('requested generation finishes once after navigation without repainting the other meeting',async()=>{
 const w=workflow();w.navigate('A');await flush();let finish;
 w.respond(req=>req.action==='generate'?new Promise(resolve=>{finish=resolve;}):savedState());
 const creating=w.button('Create & download .md').click();
 w.navigate('B');await flush();const shown=w.find('textarea');
 finish(savedState());await creating;
 assert.equal(w.find('textarea'),shown);assert.equal(w.downloads.length,1);
 w.navigate('A');await flush();assert.equal(w.downloads.length,1);
});
test('a failed offscreen save keeps the unsaved text available for retry',async()=>{
 const w=workflow();w.respond(()=>savedState());w.navigate('A');await flush();
 const area=w.find('textarea');area.value='Keep this unsaved text';area.listeners.input();let finish;
 w.respond(req=>req.action==='save'?new Promise(resolve=>{finish=resolve;}):savedState());
 const saving=w.button('Save & download .md').click();
 w.navigate('B');await flush();finish({error:'Save conflict'});await saving;
 w.navigate('A');await flush();
 assert.equal(w.find('textarea').value,'Keep this unsaved text');
 assert.ok(w.button('Save & download .md'));assert.equal(w.downloads.length,0);
});
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
test('a spinner shows while the document is being made and stops once the download starts', async () => {
  const w = workflow(); w.mount('meeting');
  assert.equal(w.find('span')?.className, 'document-spinner', 'loading the saved draft shows the status spinner');
  assert.doesNotMatch(w.button('Create & download .md').className, /is-busy/, 'a load is not a download');
  await flush();
  assert.equal(w.find('span'), undefined, 'an idle panel has no spinner');
  let finish;
  w.respond(req => req.action === 'generate' ? new Promise(resolve => {finish = resolve;}) : savedState());
  const creating = w.button('Create & download .md').click();
  assert.equal(w.find('span')?.className, 'document-spinner');
  assert.match(w.button('Create & download .md').className, /\bis-busy\b/);
  finish(savedState()); await creating;
  assert.equal(w.downloads.length, 1);
  assert.equal(w.find('span'), undefined, 'the spinner stops once the file is handed to the browser');
  assert.doesNotMatch(w.button('Download .md').className, /is-busy/);
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

test('sharing excerpts retain the full transcript speech origin after trimming', () => {
  const e=editor();
  e.document.speech_origin_ms=17000;
  e.document.excerpts=[{index:2,offset_ms:36000,at_ms:Date.parse('2026-07-14T14:05:48Z'),speaker:'Call audio',text:'later speech'}];
  const md=context.sharingMarkdown(e);
  assert.match(md,/00:00:19/);
  assert.match(md,/10:05:48 AM EDT/);
  assert.match(md,/Time since first speech/);
  assert.equal(e.document.excerpts[0].offset_ms,36000);
});

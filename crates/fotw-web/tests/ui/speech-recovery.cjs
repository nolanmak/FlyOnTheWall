const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function rig() {
  class Node {
    constructor(tag) { this.tagName=tag;this.children=[];this.listeners={};this.hidden=false;this.disabled=false;this.textContent=''; }
    appendChild(n) { n.parentNode=this;this.children.push(n);return n; }
    set disabled(value) { this._disabled=value;if(value && focused===this) focused=body; }
    get disabled() { return this._disabled; }
    get firstChild() { return this.children[0]; }
    removeChild(n) { this.children=this.children.filter(c=>c!==n);n.parentNode=null; }
    get isConnected() { return this===body || !!this.parentNode?.isConnected; }
    setAttribute(k,v) { this[k]=v; }
    addEventListener(k,fn) { this.listeners[k]=fn; }
    async click() { if (!this.disabled) return this.listeners.click?.(); }
    focus() { focused=this; }
  }
  let focused;
  const body=new Node('body'), timers=[], requests=[];
  let respond=()=>status(), reloads=0;
  const ctx=vm.createContext({document:{createElement:tag=>new Node(tag)},setTimeout:fn=>{timers.push(fn);}});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/speech-recovery.js'),'utf8'),ctx);
  const api=async(url,options)=>{requests.push({url,options});return respond(url,options);};
  const all=(n=body)=>[n,...n.children.flatMap(c=>all(c))];
  return {body,timers,requests,respond:fn=>{respond=fn;},mount:id=>ctx.FOTWSpeechRecovery.mount(id,body,api,()=>{reloads++;}),
    button:label=>all().find(n=>n.tagName==='button'&&n.textContent===label),all,focused:()=>focused,reloads:()=>reloads};
}
function status(phase='incomplete') { return {phase,worker_running:['preparing','transcribing','stopping'].includes(phase),can_retry:phase==='incomplete',sources:[{source:'system',model:'fixture',pending_audio_ms:2500}],message:null}; }
const flush=()=>new Promise(resolve=>setImmediate(resolve));
test('partial speech recovery offers a reload while retaining the interrupted recording warning',async()=>{
  const w=rig();w.respond(()=>({...status('partial'),message:'Available saved speech recovered. The final recording time remains unknown and original audio is retained.'}));
  const root=w.mount('partial');await flush();
  assert.equal(root.hidden,false);
  assert.ok(w.all().some(n=>n.tagName==='h3'&&n.textContent==='Saved speech recovered'));
  assert.ok(!w.all().some(n=>n.textContent==='Transcription recovered'));
  assert.equal(w.button('Reload transcript').hidden,false);
  assert.equal(w.button('Retry locally').disabled,true);
  assert.equal(w.timers.length,0);
  await w.button('Reload transcript').click();assert.equal(w.reloads(),1);
});
test('interrupted recordings explain the unknown end without offering inference or polling',async()=>{
  const w=rig();w.respond(()=>({...status('retained'),sources:[]}));
  const root=w.mount('interrupted');await flush();
  assert.equal(root.hidden,false);
  assert.ok(w.all().some(n=>n.tagName==='h3'&&n.textContent==='Interrupted recording'));
  assert.ok(w.all().some(n=>/final recording time is unknown/i.test(n.textContent)));
  assert.equal(w.button('Retry locally').disabled,true);
  assert.equal(w.button('Cancel retry').hidden,true);
  assert.equal(w.button('Reload transcript').hidden,true);
  assert.equal(w.timers.length,0);
  await w.button('Retry locally').click();assert.equal(w.requests.length,1);
});
test('opening only reads metadata; retry and cancellation require explicit actions',async()=>{
  const w=rig();w.mount('meeting');await flush();
  assert.equal(w.requests.length,1);assert.equal(w.requests[0].options,undefined);
  assert.ok(w.button('Retry locally'));assert.equal(w.button('Retry locally').disabled,false);
  w.respond(()=>status('preparing'));await w.button('Retry locally').click();
  assert.equal(w.requests[1].url,'/api/meetings/meeting/speech-recovery/retry');
  assert.equal(w.requests[1].options.method,'POST');assert.equal(w.requests[1].options.body,'{}');
  assert.equal(w.requests[1].options.headers?.['content-type'],'application/json');
  assert.equal(w.button('Retry locally').disabled,true);assert.equal(w.button('Cancel retry').disabled,false);
  w.respond(()=>status('stopping'));await w.button('Cancel retry').click();
  assert.match(w.requests[2].url,/\/cancel$/);assert.equal(w.button('Retry locally').disabled,true);
});
test('late replies and detached polling never affect a different meeting',async()=>{
  const w=rig();let finish;w.respond(()=>new Promise(resolve=>{finish=resolve;}));
  const root=w.mount('old');w.body.removeChild(root);finish(status('transcribing'));await flush();
  assert.equal(w.timers.length,0);assert.equal(w.requests.length,1);
  w.respond(()=>status());w.mount('new');await flush();assert.equal(w.requests.length,2);
  assert.equal(w.button('Retry locally').disabled,false);
});
test('native cleanup keeps retry disabled, completed work offers an explicit reload',async()=>{
  const w=rig();w.respond(()=>status('preparing'));w.mount('meeting');await flush();
  assert.equal(w.button('Retry locally').disabled,true);assert.equal(w.timers.length,1);
  w.respond(()=>status('stopping'));await w.timers.shift()();assert.equal(w.button('Retry locally').disabled,true);
  w.respond(()=>status('complete'));await w.timers.shift()();
  assert.equal(w.reloads(),0);assert.equal(w.button('Reload transcript').hidden,false);
  await w.button('Reload transcript').click();assert.equal(w.reloads(),1);
});
test('untrusted model text is inert, unavailable controls hide, transient failures offer refresh',async()=>{
  const w=rig();w.respond(()=>({...status(),sources:[{source:'system',model:'<script>model</script>',pending_audio_ms:10}]}));
  const root=w.mount('meeting');await flush();assert.ok(w.all().some(n=>n.textContent.includes('<script>model</script>')));
  w.respond(()=>{throw Object.assign(new Error('fixture'),{status:500});});await w.button('Refresh status').click();
  assert.equal(w.button('Retry locally').disabled,true);assert.equal(w.button('Refresh status').disabled,false);
  w.respond(()=>{throw Object.assign(new Error('absent'),{status:404});});await w.button('Refresh status').click();assert.equal(root.hidden,true);
});

test('metadata refresh preserves the focused retry control',async()=>{
  const w=rig();w.mount('meeting');await flush();const retry=w.button('Retry locally');retry.focus();
  await w.button('Refresh status').click();assert.equal(w.focused(),retry);
});

const test=require('node:test'),assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm'),path=require('node:path');
function rig(api,session=false) {
 class Element {
  constructor(tag){this.tag=tag;this.children=[];this.listeners={};this.nativeValue='';this.selectionStart=0;this.selectionEnd=0;this.isConnected=true;this.writes=[];}
  get value(){return this.nativeValue;}set value(v){this.writes.push(v);this.nativeValue=this.tag==='textarea'?String(v).replace(/\r\n?/g,'\n'):v;}
  appendChild(n){this.children.push(n);return n;}setAttribute(k,v){this[k]=v;}addEventListener(k,f){this.listeners[k]=f;}
  focus(){}setSelectionRange(a,b){this.selectionStart=a;this.selectionEnd=b;}
  contains(n){return this===n||this.children.some(c=>c.contains(n));}
 }
 const timers=new Map(),windowEvents=new Map();let serial=0,anchorIds=0,now=100;
 const ctx=vm.createContext({document:{createElement:tag=>new Element(tag)},window:{addEventListener:(name,fn)=>windowEvents.set(name,fn)},setTimeout:f=>{timers.set(++serial,f);return serial;},clearTimeout:id=>timers.delete(id)});
 vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/notes.js'),'utf8'),ctx);
 const parent=new Element('div'),notes=ctx.FOTWNotes;
 const options=session?{session:true,clock:()=>now,newId:()=> 'ime-block-'+(++anchorIds)}:{};
 const draft=notes.mount('ime-A',parent,api,options),section=parent.children[0],input=section.children[1].children[0];
 const fire=(event,data={})=>input.listeners[event]?.(data);
 const type=(value,composing=false)=>{fire('beforeinput');input.nativeValue=value;fire('input',{isComposing:composing});};
 return {ctx,parent,notes,draft,section,input,fire,type,timers,windowEvents,now:v=>now=v,runTimers(){const list=[...timers.values()];timers.clear();for(const f of list)f();}};
}
const empty={revision:0,body_md:'',anchors:[]};
const settle=async()=>{for(let i=0;i<8;i++)await Promise.resolve();};
test('IME preedit never autosaves; commit saves one complete body and the first input clock',async()=>{
 const writes=[],h=rig(async(_,opts)=>{if(!opts)return {note:empty,meeting_id:null};const p=JSON.parse(opts.body);writes.push(p);return {note:{...p,revision:1},meeting_id:null};},true);await settle();
 h.now(125);h.fire('compositionstart');h.type('に',true);h.now(900);h.type('日本',true);h.runTimers();await settle();
 assert.equal(writes.length,0,'autosave must not publish preedit candidates');
 assert.match(h.section.children[2].textContent,/Compos/);
 h.input.nativeValue='日本語';h.fire('compositionend');await settle();
 assert.equal(writes.length,1);assert.equal(writes[0].body_md,'日本語');assert.equal(writes[0].anchors[0].typed_at_ms,125);
 assert.equal(h.input.value,'日本語');assert.equal(h.draft.status,'saved');
 h.type('日本語');h.runTimers();await settle();assert.equal(writes.length,1,'the post-composition input must not duplicate the save');
});
test('an earlier save response cannot replace active native preedit or drain its new text',async()=>{
 let release;const writes=[],h=rig(async(_,opts)=>{if(!opts)return empty;const p=JSON.parse(opts.body);writes.push(p);if(writes.length===1)await new Promise(r=>release=r);return {...p,revision:writes.length};});await settle();
 h.type('previous');const saving=h.draft.flush();h.fire('compositionstart');h.input.nativeValue='previousに';
 release();await saving;assert.equal(h.input.value,'previousに','status repaint must not assign to a composing textarea');
 h.type('previous日本',true);h.runTimers();await settle();assert.equal(writes.length,1,'preedit must not drain through the previous save loop');
 h.fire('compositionend');await settle();assert.equal(writes.at(-1).body_md,'previous日本');
});
test('navigation flushes the latest visible composition to its original identity and releases stale handlers',async()=>{
 const writes=[],h=rig(async(url,opts)=>{if(!opts)return empty;const p=JSON.parse(opts.body);writes.push([url,p]);return {...p,revision:writes.length};});await settle();
 h.fire('compositionstart');h.type('に',true);h.input.nativeValue='日本';
 await h.notes.flushAll();assert.equal(writes.at(-1)[1].body_md,'日本','navigation must capture the native value even before its input callback');
 h.notes.release(h.parent);await settle();
 h.input.nativeValue='late detached candidate';h.fire('compositionend');h.type('late event');h.runTimers();await settle();
 assert.ok(writes.every(([url,p])=>url==='/api/meetings/ime-A/notes'&&p.body_md!=='late event'));
 assert.equal(h.draft.body,'日本');
});
test('block movement and destructive conflict actions stay disabled during composition',async()=>{
 const h=rig(async()=>({revision:1,body_md:'one\n\ntwo',anchors:[]}));await settle();
 h.input.selectionStart=5;h.fire('compositionstart');const actions=h.section.children[3];
 assert.equal(actions.children[3].disabled,true);assert.equal(actions.children[4].disabled,true);
 h.draft.status='conflict';h.draft.dirty=true;h.draft.changed();assert.equal(actions.children[1].disabled,true);assert.equal(actions.children[2].disabled,true);
});


test('reload responses cannot erase edits or preedit started while the read was in flight',async()=>{
 let reads=0,release;const h=rig(async()=>{if(++reads===1)return empty;return new Promise(r=>release=r);});await settle();
 const loading=h.draft.load();h.fire('compositionstart');h.type('新しい',true);
 release({revision:4,body_md:'stale server text',anchors:[]});await loading;
 assert.equal(h.draft.body,'新しい');assert.equal(h.input.value,'新しい');assert.equal(h.draft.revision,0);
});
test('a composition committing between save-loop completion and pending cleanup still drains once',async()=>{
 let release;const writes=[],h=rig(async(_,opts)=>{if(!opts)return empty;const p=JSON.parse(opts.body);writes.push(p);if(writes.length===1)await new Promise(r=>release=r);return {...p,revision:writes.length};});await settle();
 h.type('old');const saving=h.draft.flush();h.fire('compositionstart');h.type('oldに',true);
 const notify=h.draft.notify;let queued=false;
 h.draft.notify=()=>{notify();if(!queued&&h.draft.status==='unsaved'&&h.draft.composing){queued=true;Promise.resolve().then(()=>{h.input.nativeValue='old日本';h.fire('compositionend');});}};
 release();await saving;await settle();assert.equal(writes.length,2);assert.equal(writes[1].body_md,'old日本');assert.equal(h.draft.dirty,false);
});
test('an uncommitted native composition warns before unload even before an input event',async()=>{
 const h=rig(async()=>empty);await settle();let warned=false;
 h.fire('compositionstart');h.windowEvents.get('beforeunload')({preventDefault(){warned=true;}});assert.equal(warned,true);
 h.fire('compositionend');warned=false;h.windowEvents.get('beforeunload')({preventDefault(){warned=true;}});assert.equal(warned,false);
});

test('native deleteCompositionText then insertFromComposition retains the first keystroke anchor',async()=>{
 const writes=[],h=rig(async(_,opts)=>{if(!opts)return {note:empty,meeting_id:null};const p=JSON.parse(opts.body);writes.push(p);return {note:{...p,revision:1},meeting_id:null};},true);await settle();
 const trace=require('../fixtures/notes-ime-webkit.json');h.now(125);
 for(const event of trace.events) {
  if(event.event==='compositionupdate'&&event.value==='に')h.now(5900);
  h.input.nativeValue=event.value;h.fire(event.event,event);
 }
 await settle();
 assert.equal(writes.length,1);assert.equal(writes[0].body_md,'日本語');assert.equal(writes[0].anchors[0].typed_at_ms,125,'native transient deletion must not reset the block clock');
});

test('late final capture bounds cannot be resurrected from the composition checkpoint',async()=>{
 let finalized=false;const writes=[],h=rig(async(_,opts)=>{
  if(!opts)return {note:empty,meeting_id:finalized?'saved-A':null,elapsed_ms:finalized?500:null};
  const p=JSON.parse(opts.body);writes.push(p);return {note:{...p,revision:writes.length},meeting_id:'saved-A',elapsed_ms:500};
 },true);await settle();
 h.now(1000);h.fire('compositionstart');h.type('に',true);assert.equal(h.draft.anchors.length,1);
 finalized=true;await h.notes.finishSession('ime-A','saved-A');assert.equal(h.draft.anchors.length,0);
 h.type('日本',true);assert.equal(h.draft.anchors.length,0,'composition must not restore a timestamp beyond the real capture end');
 h.fire('compositionend');await settle();assert.equal(writes.at(-1).anchors.length,0);assert.equal(writes.at(-1).body_md,'日本');
});


test('multiline composition and cancellation preserve old unanchored paragraphs and literal Unicode',async()=>{
 const writes=[],h=rig(async(_,opts)=>{if(!opts)return {note:{revision:0,body_md:'旧い段落',anchors:[]},meeting_id:null};const p=JSON.parse(opts.body);writes.push(p);return {note:{...p,revision:1},meeting_id:null};},true);await settle();
 h.now(125);h.fire('compositionstart');h.type('旧い段落\n\n新しい😀\n\n第二行',true);const ids=Array.from(h.draft.anchors,a=>a.id);
 h.now(5500);h.input.nativeValue='';h.fire('input',{isComposing:true,inputType:'deleteCompositionText'});
 h.type('旧い段落\n\n新しい😀\n\n第二行',true);h.fire('compositionend');await settle();
 assert.deepEqual(writes[0].anchors.map(a=>[a.block_idx,a.typed_at_ms]),[[1,125],[2,125]]);assert.deepEqual(writes[0].anchors.map(a=>a.id),ids);
 h.fire('compositionstart');h.type('旧い段落\n\n新しい😀\n\n第二行候補',true);h.input.nativeValue='旧い段落\n\n新しい😀\n\n第二行';h.fire('compositionend');await settle();
 assert.equal(writes.at(-1).body_md,'旧い段落\n\n新しい😀\n\n第二行');assert.equal(h.draft.anchors.length,2);
});

test('beforeinput composition metadata protects native text even if compositionstart was not observed',async()=>{
 let release;const writes=[],h=rig(async(_,opts)=>{if(!opts)return empty;const p=JSON.parse(opts.body);writes.push(p);if(writes.length===1)await new Promise(r=>release=r);return {...p,revision:writes.length};});await settle();
 h.type('old');const saving=h.draft.flush();h.fire('beforeinput',{isComposing:true});h.input.nativeValue='oldに';
 release();await saving;assert.equal(h.input.value,'oldに');assert.match(h.section.children[2].textContent,/Compos/);
 h.fire('input',{isComposing:true,inputType:'insertCompositionText'});h.input.nativeValue='old日本';h.fire('compositionend');await settle();
 assert.equal(writes.length,2);assert.equal(writes[1].body_md,'old日本');
});


test('navigation never rewrites untouched CRLF notes normalized by the native textarea',async()=>{
 const original='Café\r\n\r\nSecond paragraph\r\n',writes=[];
 const h=rig(async(_,opts)=>{if(!opts)return {revision:7,body_md:original,anchors:[]};const p=JSON.parse(opts.body);writes.push(p);return {...p,revision:8};});await settle();
 assert.equal(h.input.value,'Café\n\nSecond paragraph\n');
 await h.notes.flushAll();await h.notes.release(h.parent);
 assert.equal(writes.length,0,'merely visiting or leaving notes must not save browser line-ending normalization');assert.equal(h.draft.body,original);assert.equal(h.draft.revision,7);
});

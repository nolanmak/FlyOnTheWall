const test=require('node:test');
const assert=require('node:assert/strict');
const {Draft}=require('../../ui/notes.js');
const snapshot=(revision=0)=>({revision,body_md:'original',anchors:[]});
test('edits during a save remain dirty and flush to the original meeting',async()=>{
 let release; const writes=[];
 const api=async(path,opts)=>{if(!opts)return snapshot();const d=JSON.parse(opts.body);writes.push([path,d]);if(writes.length===1)await new Promise(r=>release=r);return {...d,revision:d.revision+1};};
 const a=new Draft('A',api);await a.load();a.edit('first');const saving=a.flush();
 a.edit('latest');release();await saving;
 assert.equal(writes.length,2);assert.equal(writes[1][0],'/api/meetings/A/notes');
 assert.equal(writes[1][1].body_md,'latest');assert.equal(writes[1][1].revision,1);
 assert.equal(a.dirty,false);assert.equal(a.status,'saved');
});
test('network failure retains draft, reports error, and permits retry',async()=>{
 let fails=true;
 const d=new Draft('B',async(_,opts)=>{if(!opts)return snapshot();if(fails)throw new Error('offline');return {...JSON.parse(opts.body),revision:1};});
 await d.load();d.edit('unsaved');await d.flush();
 assert.equal(d.body,'unsaved');assert.equal(d.dirty,true);assert.equal(d.status,'error');
 fails=false;await d.flush();assert.equal(d.dirty,false);assert.equal(d.status,'saved');
});
test('conflict preserves user text until explicit reload or overwrite',async()=>{
 let loads=0,writes=0;
 const d=new Draft('C',async(_,opts)=>{if(!opts)return snapshot(loads++);writes++;if(writes===1)throw Object.assign(new Error('conflict'),{status:409});return {...JSON.parse(opts.body),revision:2};});
 await d.load();d.edit('my change');await d.flush();
 assert.equal(d.status,'conflict');assert.equal(d.body,'my change');
 await d.flush();assert.equal(writes,1,'no silent retry over another editor');
 await d.overwrite();assert.equal(d.body,'my change');assert.equal(d.revision,2);assert.equal(d.dirty,false);
});

test('mounted editor offers enabled explicit conflict resolution and preserves literal text',async()=>{
 class Element {
  constructor(tag){this.tag=tag;this.children=[];this.listeners={};this.value='';}
  appendChild(n){this.children.push(n);return n;}
  setAttribute(k,v){this[k]=v;}
  addEventListener(k,f){this.listeners[k]=f;}
 }
 global.document={createElement:tag=>new Element(tag)};
 const parent=new Element('div');
 const {mount}=require('../../ui/notes.js');
 mount('mounted',parent,async(_,opts)=>{if(!opts)return snapshot();throw Object.assign(new Error('conflict'),{status:409});});
 await new Promise(setImmediate);
 const section=parent.children[0],input=section.children[1].children[0],actions=section.children[3];
 input.value='<script>literal note</script>';input.listeners.input();await input.listeners.blur();
 assert.equal(input.value,'<script>literal note</script>');
 assert.equal(actions.children[1].hidden,false);
 assert.equal(actions.children[2].disabled,false,'discard/reload must be usable after conflict');
 delete global.document;
});

const anchoredSnapshot=()=>({revision:1,body_md:'First point\n\nSecond point',anchors:[
 {id:'first',block_idx:0,block_text:'First point',typed_at_ms:100},
 {id:'second',block_idx:1,block_text:'Second point',typed_at_ms:200}
]});
test('editing a paragraph updates its anchor text and preserves its identity and first time',async()=>{
 const d=new Draft('anchored-edit',async()=>anchoredSnapshot());await d.load();
 d.edit('First important point\n\nSecond point');
 assert.deepEqual(d.anchors,[{id:'first',block_idx:0,block_text:'First important point',typed_at_ms:100},anchoredSnapshot().anchors[1]]);
});
test('reorder follows the block, deletion removes it, and recreation has no historical time',async()=>{
 const d=new Draft('anchored-reorder',async()=>anchoredSnapshot());await d.load();
 d.edit('Second point\n\nFirst point');
 assert.deepEqual(d.anchors.map(a=>[a.id,a.block_idx,a.typed_at_ms]),[['second',0,200],['first',1,100]]);
 d.edit('Second point');assert.deepEqual(d.anchors.map(a=>a.id),['second']);
 d.edit('Second point\n\nFirst point');assert.deepEqual(d.anchors.map(a=>a.id),['second']);
});
test('inserting an unanchored paragraph shifts old anchors without inventing a timestamp',async()=>{
 const d=new Draft('anchored-insert',async()=>anchoredSnapshot());await d.load();
 d.edit('New paragraph\n\nFirst point\n\nSecond point');
 assert.deepEqual(d.anchors.map(a=>[a.id,a.block_idx]),[['first',1],['second',2]]);
});
test('editing the first of identical paragraphs does not steal the second timestamp',async()=>{
 const d=new Draft('duplicates',async()=>({revision:1,body_md:'same\n\nsame',anchors:[
  {id:'a',block_idx:0,block_text:'same',typed_at_ms:100},
  {id:'b',block_idx:1,block_text:'same',typed_at_ms:200}]}));await d.load();
 d.edit('same edited\n\nsame');
 assert.deepEqual(d.anchors.map(a=>[a.id,a.block_text,a.typed_at_ms]),[['a','same edited',100],['b','same',200]]);
});
test('splitting retains the original anchor on the first block; merging keeps the leading block anchor',async()=>{
 const d=new Draft('split',async()=>({revision:1,body_md:'alpha beta',anchors:[{id:'a',block_idx:0,block_text:'alpha beta',typed_at_ms:100}]}));await d.load();
 d.edit('alpha\n\nbeta');assert.deepEqual(d.anchors,[{id:'a',block_idx:0,block_text:'alpha',typed_at_ms:100}]);
 d.edit('alpha beta');assert.deepEqual(d.anchors,[{id:'a',block_idx:0,block_text:'alpha beta',typed_at_ms:100}]);
});
test('block spans preserve literal Markdown, fenced blank lines, list items and Unicode',()=>{
 const {blocks}=require('../../ui/notes.js');
 const body='# Heading\n\n- café\n- second\n\n```js\nconst x = "😀";\n\n```\n\nlast\nline';
 assert.deepEqual(blocks(body).map(b=>b.text),['# Heading','- café','- second','```js\nconst x = "😀";\n\n```','last\nline']);
 for(const b of blocks(body))assert.equal(body.slice(b.start,b.end),b.text);
});
test('selection evidence distinguishes deleting the first of identical paragraphs',async()=>{
 const d=new Draft('duplicate-delete',async()=>({revision:1,body_md:'same\n\nsame',anchors:[
  {id:'a',block_idx:0,block_text:'same',typed_at_ms:100},
  {id:'b',block_idx:1,block_text:'same',typed_at_ms:200}]}));await d.load();
 d.edit('same',{start:0,end:6});
 assert.deepEqual(d.anchors,[{id:'b',block_idx:0,block_text:'same',typed_at_ms:200}]);
});
test('explicit movement preserves distinct identities even when both blocks have identical text',async()=>{
 const d=new Draft('duplicate-move',async()=>({revision:1,body_md:'same\n\nsame',anchors:[
  {id:'a',block_idx:0,block_text:'same',typed_at_ms:100},
  {id:'b',block_idx:1,block_text:'same',typed_at_ms:200}]}));await d.load();
 assert.equal(d.moveBlock(1,-1),true);
 assert.equal(d.body,'same\n\nsame');
 assert.deepEqual(d.anchors.map(a=>[a.id,a.block_idx]),[['b',0],['a',1]]);
 assert.equal(d.dirty,true);assert.equal(d.moveBlock(0,-1),false);
});
test('edits during save serialize the matching body and anchor snapshots together',async()=>{
 let release;const writes=[];
 const d=new Draft('anchor-flight',async(_,opts)=>{
  if(!opts)return anchoredSnapshot();
  const payload=JSON.parse(opts.body);writes.push(payload);
  if(writes.length===1)await new Promise(r=>release=r);
  return {...payload,revision:payload.revision+1};
 });await d.load();d.edit('First edited\n\nSecond point');const saving=d.flush();
 d.moveBlock(0,1);release();await saving;
 assert.equal(writes.length,2);
 assert.equal(writes[0].body_md,'First edited\n\nSecond point');
 assert.deepEqual(writes[0].anchors.map(a=>[a.id,a.block_text]),[['first','First edited'],['second','Second point']]);
 assert.equal(writes[1].body_md,'Second point\n\nFirst edited');
 assert.deepEqual(writes[1].anchors.map(a=>[a.id,a.block_idx]),[['second',0],['first',1]]);
});
test('mounted movement controls move the selected block and restore keyboard focus',async()=>{
 class Element {
  constructor(tag){this.tag=tag;this.children=[];this.listeners={};this.value='';this.selectionStart=0;this.selectionEnd=0;}
  appendChild(n){this.children.push(n);return n;}
  setAttribute(k,v){this[k]=v;}
  addEventListener(k,f){this.listeners[k]=f;}
  focus(){this.focused=true;}
  setSelectionRange(a,b){this.selectionStart=a;this.selectionEnd=b;}
 }
 global.document={createElement:tag=>new Element(tag)};
 try {
  const writes=[],parent=new Element('div');
  require('../../ui/notes.js').mount('mounted-move',parent,async(_,opts)=>{
   if(!opts)return anchoredSnapshot();const payload=JSON.parse(opts.body);writes.push(payload);return {...payload,revision:2};
  });await new Promise(setImmediate);
  const section=parent.children[0],input=section.children[1].children[0],actions=section.children[3];
  assert.equal(actions.children[3].disabled,true);assert.equal(actions.children[4].disabled,false);
  actions.children[4].listeners.click();await new Promise(setImmediate);
  assert.equal(input.value,'Second point\n\nFirst point');assert.equal(input.focused,true);
  assert.equal(input.selectionStart,'Second point\n\n'.length);
  assert.deepEqual(writes[0].anchors.map(a=>a.id),['second','first']);
 } finally {delete global.document;}
});
test('merging two anchored paragraphs keeps the leading identity, not a later timestamp',async()=>{
 const d=new Draft('two-merge',async()=>anchoredSnapshot());await d.load();
 d.edit('First point Second point');
 assert.deepEqual(d.anchors,[{id:'first',block_idx:0,block_text:'First point Second point',typed_at_ms:100}]);
});

test('live typing anchors immediately and retains its clock through edits, split, paste and stop',async()=>{
 let now=125, serial=0, recording=true;const writes=[];
 const empty={revision:0,body_md:'',anchors:[]};
 const wrap=note=>({session_id:'live-A',meeting_id:null,started_at_ms:1000,elapsed_ms:5000,note});
 const api=async(path,opts)=>{if(!opts)return wrap(empty);const doc=JSON.parse(opts.body);writes.push([path,doc]);return {...wrap({...doc,revision:doc.revision+1}),meeting_id:'saved-A'};};
 const d=new Draft('live-A',api,()=>{},{session:true,clock:()=>recording?now:null,newId:()=>`live-${++serial}`});
 await d.load();d.edit('F');
 assert.equal(d.anchors[0]?.typed_at_ms,125,'anchor at input, not delayed autosave');
 now=900;d.edit('First');assert.equal(d.anchors[0].typed_at_ms,125);
 d.edit('First\n\nSecond\n\nThird');
 assert.deepEqual(d.anchors.map(a=>a.typed_at_ms),[125,900,900]);
 const first=d.anchors[0].id;d.moveBlock(0,1);assert.equal(d.anchors[1].id,first);
 d.edit('Second\n\nFirst');now=1000;d.edit('Second\n\nFirst\n\nThird');
 assert.equal(d.anchors[2].typed_at_ms,1000,'recreated block has a new first time');
 recording=false;d.edit(d.body+'\n\nAfter stop');assert.equal(d.anchors.length,3);
 await d.flush();assert.equal(d.revision,1);assert.equal(d.status,'saved');
 assert.equal(writes[0][0],'/api/sessions/live-A/notes');assert.equal(d.savedMeetingId,'saved-A');
});

test('finalization-gap responses keep a retryable live draft, not an overwrite conflict',async()=>{
 let pending=true;
 const d=new Draft('gap',async(_,opts)=>{
  if(!opts)return {note:{revision:0,body_md:'',anchors:[]},meeting_id:null};
  if(pending)throw Object.assign(new Error('finishing'),{status:409,code:'recording_clock_unavailable'});
  return {note:{...JSON.parse(opts.body),revision:1},meeting_id:'saved-gap'};
 },()=>{},{session:true,clock:()=>null});
 await d.load();d.edit('keep me');await d.flush();
 assert.equal(d.status,'error');assert.equal(d.dirty,true);
 pending=false;await d.flush();assert.equal(d.status,'saved');assert.equal(d.body,'keep me');
});

test('the live clock uses capture offsets across sleep, clamps rollback, and stops at capture bounds',()=>{
 const {RecordingClock}=require('../../ui/notes.js');let now=10000;const c=new RecordingClock(()=>now);
 c.update({state:'recording',session_id:'clock-A',started_at_ms:1000,elapsed_ms:125,auto_stop_at_ms:11000});
 assert.equal(c.offset('clock-A'),125);assert.equal(c.offset('wrong'),null);
 now+=1000;assert.equal(c.offset('clock-A'),1125);
 now-=2000;assert.equal(c.offset('clock-A'),125,'clock rollback cannot produce negative time');
 now=11000;c.stop();assert.equal(c.offset('clock-A'),null);
 c.update({state:'recording',session_id:'clock-A',started_at_ms:1000,elapsed_ms:1200});
 assert.equal(c.offset('clock-A'),null,'poll during Stop must not re-enable anchors');
 c.retry();c.update({state:'recording',session_id:'clock-A',started_at_ms:1000,elapsed_ms:1300,auto_stop_at_ms:3000});
 now+=1000;assert.equal(c.offset('clock-A'),null,'no anchors beyond automatic stop');
 c.update({state:'finishing',session_id:'clock-A',started_at_ms:1000,elapsed_ms:2000});
 assert.equal(c.offset('clock-A'),null);
 c.update({state:'recording',session_id:'clock-B',started_at_ms:5000,elapsed_ms:0});
 assert.equal(c.offset('clock-A'),null);assert.equal(c.offset('clock-B'),0);
});

test('editing recovered unanchored text does not fabricate its first-keystroke time',async()=>{
 const d=new Draft('recovered',async()=>({note:snapshot(),meeting_id:null}),()=>{},
  {session:true,clock:()=>1234,newId:()=> 'new-block'});
 await d.load();d.edit('original edited');assert.equal(d.anchors.length,0);
 d.edit('original edited\n\nNew block');assert.deepEqual(d.anchors.map(a=>[a.block_idx,a.typed_at_ms]),[[1,1234]]);
});

test('saved navigation reuses the live draft while its original session save is in flight',async()=>{
 class Element {constructor(tag){this.children=[];this.listeners={};this.value='';this.dataset={};}
  appendChild(n){this.children.push(n);return n;}setAttribute(){}addEventListener(k,f){this.listeners[k]=f;}}
 global.document={createElement:tag=>new Element(tag)};
 try {
  const notes=require('../../ui/notes.js');let release;const writes=[];
  const api=async(path,opts)=>{if(!opts)return {note:{revision:0,body_md:'',anchors:[]},meeting_id:null};
   const d=JSON.parse(opts.body);writes.push([path,d]);if(writes.length===1)await new Promise(r=>release=r);
   return {note:{...d,revision:d.revision+1},meeting_id:'alias-saved'};};
  const live=notes.mount('alias-live',new Element(),api,{session:true,clock:()=>100,newId:()=> 'alias-block'});
  await new Promise(setImmediate);live.edit('draft before stop');const pending=live.flush();
  notes.linkSession('alias-live','alias-saved');
  const saved=notes.mount('alias-saved',new Element(),api);
  assert.equal(saved,live);assert.equal(saved.body,'draft before stop');
  saved.edit('edited while finalizing');release();await pending;
  assert.deepEqual(writes.map(w=>w[0]),['/api/sessions/alias-live/notes','/api/sessions/alias-live/notes']);
  assert.equal(writes[1][1].revision,1);assert.equal(saved.dirty,false);
 }finally{delete global.document;}
});

test('late final clock removes only beyond-end anchors and retains text on retry',async()=>{
 let finalized=false;const writes=[];
 const d=new Draft('late-bound',async(_,opts)=>{
  const note={revision:0,body_md:'',anchors:[]};
  if(!opts)return {note,meeting_id:finalized?'saved-bound':null,elapsed_ms:500};
  const payload=JSON.parse(opts.body);writes.push(payload);
  if(!finalized)throw Object.assign(new Error('gap'),{status:409,code:'recording_clock_unavailable'});
  return {note:{...payload,revision:payload.revision+1},meeting_id:'saved-bound',elapsed_ms:500};
 },()=>{},{session:true,clock:()=>600,newId:()=> 'late-block'});
 await d.load();d.edit('text typed after unknown stop');await d.flush();
 assert.equal(d.anchors.length,1);finalized=true;await d.retrySave();
 assert.equal(d.body,'text typed after unknown stop');assert.equal(writes.at(-1).anchors.length,0);
 d.edit(d.body+'\n\nMore after stop');assert.equal(d.anchors.length,0);
});

function recoveryDOM() {
 class Element {
  constructor(tag){this.tag=tag;this.children=[];this.listeners={};this.value='';this.ownText='';this.dataset={};}
  appendChild(n){this.children.push(n);return n;}
  set textContent(v){this.ownText=String(v);this.children=[];}
  get textContent(){return this.ownText+this.children.map(c=>c.textContent).join('');}
  setAttribute(k,v){this[k]=v;}addEventListener(k,f){this.listeners[k]=f;}
 }
 const parent=new Element('div');global.document={createElement:tag=>new Element(tag)};
 const all=(n=parent)=>[n,...n.children.flatMap(all)];
 return {parent,all};
}
test('recovery index pages metadata and retries a failed page without losing earlier rows',async()=>{
 const h=recoveryDOM();try {
  const calls=[],opened=[];let failed=false;
  require('../../ui/notes.js').mountRecovery(h.parent,async(path)=>{
   calls.push(path);if(path.includes('offset=50') && !failed){failed=true;throw new Error('temporary');}
   return {drafts:Array.from({length:path.includes('offset=50')?1:50},(_,i)=>({session_id:`draft-${calls.length}-${i}`,started_at_ms:1000+i,updated_at_ms:2000}))};
  },id=>opened.push(id));await new Promise(setImmediate);
  const row=()=>h.all().filter(n=>n.tag==='button' && n.dataset?.sessionId);
  assert.equal(row().length,50);row()[0].listeners.click();assert.deepEqual(opened,['draft-1-0']);
  const more=h.all().find(n=>n.textContent==='Load more');await more.listeners.click();
  assert.equal(row().length,50);assert.match(h.parent.textContent,/Could not load/);
  await more.listeners.click();assert.equal(row().length,51);assert.equal(more.hidden,true);
  assert.equal(calls[1],calls[2]);
 }finally{delete global.document;}
});
test('recovered text stays literal and read-only, with explicit copy and metadata-preserving download',async()=>{
 const h=recoveryDOM();try {
  const copied=[],downloads=[],calls=[];
  const doc={session_id:'orphan',meeting_id:null,started_at_ms:1000,elapsed_ms:null,note:{revision:2,body_md:'<script>literal fixture</script>\nCafé',anchors:[{id:'original',block_idx:0,block_text:'fixture',typed_at_ms:250}]}};
  require('../../ui/notes.js').mountRecovered('orphan',h.parent,async(path)=>{calls.push(path);return doc;},
   {copy:async text=>copied.push(text),download:(...args)=>downloads.push(args)});
  await new Promise(setImmediate);
  const input=h.all().find(n=>n.tag==='textarea');assert.equal(input?.value,doc.note.body_md);assert.equal(input.readOnly,true);
  assert.equal(downloads.length,0);assert.equal(copied.length,0);
  await h.all().find(n=>n.textContent==='Copy notes').listeners.click();assert.equal(copied[0],doc.note.body_md);
  h.all().find(n=>n.textContent==='Download JSON').listeners.click();
  assert.equal(JSON.parse(downloads[0][1]).note.anchors[0].id,'original');
  assert.equal(JSON.parse(downloads[0][1]).elapsed_ms,null);
  assert.deepEqual(calls,['/api/sessions/orphan/notes']);
 }finally{delete global.document;}
});

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

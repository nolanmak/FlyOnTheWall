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

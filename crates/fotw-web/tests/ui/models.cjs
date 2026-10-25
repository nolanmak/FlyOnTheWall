const test=require('node:test'),assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm'),path=require('node:path');
function rig(api) {
 class Node {
  constructor(tag){this.tagName=tag;this.children=[];this.listeners={};this.hidden=false;this.disabled=false;this.value='';this.isConnected=true;}
  appendChild(n){this.children.push(n);return n;}get firstChild(){return this.children[0];}removeChild(n){this.children.splice(this.children.indexOf(n),1);}
  get textContent(){return(this.own||'')+this.children.map(n=>n.textContent).join('');}set textContent(s){this.own=String(s);this.children=[];}
  addEventListener(k,f){this.listeners[k]=f;}setAttribute(k,v){this[k]=v;}focus(){this.focused=true;}
 }
 const timers=[],ctx=vm.createContext({document:{createElement:t=>new Node(t)},setTimeout:f=>timers.push(f)});
 vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/models.js'),'utf8'),ctx);
 const root=ctx.FOTWModels.mount(new Node('div'),api);
 const all=(n=root)=>[n,...n.children.flatMap(all)];
 const node=id=>all().find(n=>n.id===id);
 return {root,node,timers};
}
const job=(phase='idle',running=false,id=0)=>({id:String(id),model:running?'small.en':null,phase,bytes:0,total:100,worker_running:running,message:null});
const state=j=>({models:[{name:'small.en',description:'English-only',bytes:487614201,sha256:'a'.repeat(64),revision:'b'.repeat(40),license:'MIT',license_url:'https://example.test/license',present:false}],job:j||job()});
const settle=async()=>{for(let i=0;i<8;i++)await Promise.resolve();};
test('reading catalog and choosing a tier never starts a download; explicit actions show pinned metadata',async()=>{
 const calls=[],h=rig(async(url,options)=>{calls.push([url,options]);return state();});await settle();
 assert.equal(calls.length,1);assert.equal(calls[0][0],'/api/models');assert.equal(calls[0][1],undefined);
 assert.match(h.root.textContent,/MIT/);assert.match(h.root.textContent,/487614201/);assert.match(h.root.textContent,/a{64}/);
 assert.equal(h.node('model-download').disabled,false);
 h.node('model-choice').listeners.change();assert.equal(calls.length,1);
});
test('cancel uses current job identity and retains retry exclusion until owned work exits',async()=>{
 const calls=[];let current=state(job('transferring',true,7));
 const h=rig(async(url,options)=>{calls.push([url,options]);if(url.endsWith('/cancel'))current=state(job('stopping',true,7));return current;});await settle();
 assert.equal(h.node('model-download').disabled,true);assert.equal(h.node('model-import').disabled,true);
 await h.node('model-cancel').listeners.click();assert.deepEqual(JSON.parse(calls.at(-1)[1].body),{job_id:'7'});
 assert.equal(h.node('model-download').disabled,true);assert.equal(h.node('model-cancel').disabled,true);assert.match(h.node('model-status').textContent,/Waiting/);
 current=state(job('cancelled',false,7));await h.node('model-refresh').listeners.click();assert.equal(h.node('model-download').disabled,false);
});
test('imports preserve path drafts, setup completion never saves or selects a speech policy',async()=>{
 const calls=[];let current=state();const h=rig(async(url,options)=>{calls.push([url,options]);return current;});await settle();
 h.node('model-import-path').value='/draft/cache.bin';current=state(job('verified',false,9));
 await h.node('model-import').listeners.click();const sent=JSON.parse(calls.at(-1)[1].body);
 assert.deepEqual(sent,{source:'import',model:'small.en',path:'/draft/cache.bin'});assert.equal(h.node('model-import-path').value,'/draft/cache.bin');
 assert.match(h.node('model-status').textContent,/native|Start/);assert.ok(calls.every(([url])=>url.startsWith('/api/models')));
});
test('unconfirmed requests require refresh before another action and unavailable control hides the panel',async()=>{
 let broken=false;const h=rig(async()=>{if(broken)throw Object.assign(new Error('unavailable'),{status:500});return state();});await settle();
 broken=true;await h.node('model-download').listeners.click();assert.equal(h.node('model-download').disabled,true);assert.match(h.node('model-status').textContent,/Refresh/);
 const missing=rig(async()=>{throw {status:404};});await settle();assert.equal(missing.root.hidden,true);
});

test('a late status response cannot replace a newly accepted job or permit duplicate actions',async()=>{
 let stale,finish,calls=0;
 const h=rig(async(url,options)=>{calls++;if(calls===1)return state();if(!options)return new Promise(r=>stale=r);return new Promise(r=>finish=r);});await settle();
 const refresh=h.node('model-refresh').listeners.click();
 const install=h.node('model-download').listeners.click();await h.node('model-import').listeners.click();assert.equal(calls,3);
 finish(state(job('transferring',true,9)));await install;
 stale(state());await refresh;
 assert.equal(h.node('model-download').disabled,true);assert.equal(h.node('model-cancel').hidden,false);
});
test('navigation fences late responses and prevents detached polling',async()=>{
 let reply,calls=0;const h=rig(async()=>{calls++;return new Promise(r=>reply=r);});h.root.isConnected=false;
 reply(state(job('transferring',true,1)));await settle();assert.equal(h.timers.length,0);assert.equal(calls,1);
});

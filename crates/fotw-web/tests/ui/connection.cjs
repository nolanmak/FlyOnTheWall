const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');
function harness() {
  const nodes = new Map(), timers = new Map(), sockets = [];
  let nextTimer = 0;
  const node = id => {
    if (!nodes.has(id)) {
      const classes = new Set();
      nodes.set(id, {hidden:true, textContent:'', disabled:false, classList:{
        toggle(name, on) { if (on) classes.add(name); else classes.delete(name); },
        contains:name=>classes.has(name),
      }});
    }
    return nodes.get(id);
  };
  class Socket {
    constructor(url) { this.url=url; this.listeners={}; sockets.push(this); }
    addEventListener(name, callback) { this.listeners[name]=callback; }
    emit(name, data) { this.listeners[name]?.({data}); }
  }
  const ctx = vm.createContext({
    document:{getElementById:node}, window:{location:{search:'',hash:'',host:'127.0.0.1:12345'}},
    URLSearchParams, localStorage:{getItem:()=>null}, WebSocket:Socket,
    setTimeout(callback) { timers.set(++nextTimer,callback); return nextTimer; },
    clearTimeout:id=>timers.delete(id),
  });
  const source=fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8').replace(/\nmain\(\);\s*$/, '\n');
  vm.runInContext(source,ctx);
  ctx.api=async()=>({ticket:'fixture'});
  ctx.pollRecording=async()=>{};
  ctx.recoverLiveHistory=()=>{};
  return {ctx,node,sockets,timers,async retry() {
    assert.equal(timers.size,1,'exactly one retry should be scheduled');
    const [id,callback]=timers.entries().next().value; timers.delete(id); await callback();
  }};
}
test('connected updates are explicitly labelled and do not imply recording',async()=>{
  const h=harness(); await h.ctx.connectStream(); h.sockets[0].emit('open');
  assert.equal(h.node('live').hidden,false);
  assert.equal(h.node('live').textContent,'Updates connected');
  assert.equal(h.node('recording').hidden,true);
});
test('disconnect preserves recorder status, elapsed time and an available Stop',async()=>{
  const h=harness(); await h.ctx.connectStream(); h.sockets[0].emit('open');
  for(const [id,text] of [['recording','recording'],['elapsed','12:34'],['record','Stop']]) {
    Object.assign(h.node(id),{hidden:false,textContent:text,disabled:false});
  }
  h.sockets[0].emit('close');
  assert.equal(h.node('live').hidden,false);
  assert.equal(h.node('live').textContent,'Updates reconnecting…');
  for(const [id,text] of [['recording','recording'],['elapsed','12:34'],['record','Stop']]) {
    assert.equal(h.node(id).hidden,false); assert.equal(h.node(id).textContent,text);
    assert.equal(h.node(id).disabled,false);
  }
});
test('failed ticket acquisition keeps retrying and each reconnect uses a fresh ticket',async()=>{
  const h=harness(); let attempts=0;
  h.ctx.api=async()=>{if(++attempts<3)throw Error('synthetic daemon outage');return {ticket:'ticket-'+attempts};};
  await h.ctx.connectStream(); assert.equal(h.sockets.length,0);
  await h.retry(); assert.equal(h.sockets.length,0);
  await h.retry(); assert.equal(h.sockets.length,1);
  assert.match(h.sockets[0].url,/ticket=ticket-3$/);
  h.sockets[0].emit('open'); h.sockets[0].emit('close'); await h.retry();
  assert.match(h.sockets[1].url,/ticket=ticket-4$/);
});
test('concurrent requests and repeated or stale socket events cannot duplicate connections or frames',{timeout:1000},async()=>{
  const h=harness(); let resolveTicket; const frames=[];
  h.ctx.api=()=>new Promise(resolve=>resolveTicket=resolve);
  h.ctx.handleStreamFrame=frame=>frames.push(frame.kind);
  const first=h.ctx.connectStream(); await h.ctx.connectStream();
  resolveTicket({ticket:'first'}); await first;
  assert.equal(h.sockets.length,1);
  h.sockets[0].emit('open'); h.sockets[0].emit('close'); h.sockets[0].emit('close');
  h.ctx.api=async()=>({ticket:'second'}); await h.retry();
  h.sockets[1].emit('open');
  h.sockets[0].emit('open'); h.sockets[0].emit('close');
  h.sockets[0].emit('message',JSON.stringify({kind:'stale'}));
  h.sockets[1].emit('message',JSON.stringify({kind:'current'}));
  assert.deepEqual(frames,['current']);
  assert.equal(h.timers.size,0); assert.equal(h.node('live').textContent,'Updates connected');
});
test('a socket constructor failure retries instead of ending connection recovery',async()=>{
  const h=harness(); const real=h.ctx.WebSocket;
  h.ctx.WebSocket=class {constructor(){throw Error('synthetic constructor failure');}};
  await h.ctx.connectStream();
  h.ctx.WebSocket=real; await h.retry(); assert.equal(h.sockets.length,1);
});

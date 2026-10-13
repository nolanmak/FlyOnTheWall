const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function harness() {
  class Element {
    constructor(tag) { this.tagName=tag; this.children=[]; this.listeners={}; this.scrollHeight=1000;this.clientHeight=100;this.scrollTop=10;this.dataset={};this.classList={add(){},toggle(){}}; }
    get firstChild(){return this.children[0];} get childElementCount(){return this.children.length;}
    get textContent(){return (this.ownText||'')+this.children.map(c=>c.textContent).join('');}
    set textContent(t){this.ownText=String(t);this.children=[];}
    appendChild(n){n.parent=this;this.children.push(n);return n;}
    removeChild(n){this.children.splice(this.children.indexOf(n),1);n.parent=null;}
    remove(){this.parent?.removeChild(this);}
    replaceWith(n){let p=this.parent,i=p.children.indexOf(this);p.children[i]=n;n.parent=p;}
    addEventListener(k,f){this.listeners[k]=f;} setAttribute(k,v){this[k]=v;}
  }
  const root=new Element('body');
  const nodes=new Map(['detail','status','record','live','elapsed','consent','consent-label','recording','recording-limit','return-live'].map(id=>{let n=root.appendChild(new Element('div'));n.id=id;return [id,n];}));
  const all=(n=root)=>[n,...n.children.flatMap(all)];
  const document={createElement:t=>new Element(t),getElementById:id=>all().find(n=>n.id===id)||null};
  const ctx=vm.createContext({document,Date,Intl,Set,Map,URLSearchParams,window:{location:{search:''}},localStorage:{getItem:()=>null},setInterval:()=>1,clearInterval(){},setTimeout(){},XMLSerializer:class {serializeToString(n){return n.textContent;}}});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8'),ctx);
  const run=s=>vm.runInContext(s,ctx);
  return {ctx,run,document,nodes,all,old(n=2){let detail=nodes.get('detail');while(detail.firstChild)detail.removeChild(detail.firstChild);let body=detail.appendChild(new Element('div'));body.id='segments';for(let i=0;i<n;i++){let row=body.appendChild(new Element('p'));row.textContent='old '+i;}return body;}};
}
const d=(idx,start_ms,text='words',channel='system',meeting_id='B')=>({idx,start_ms,end_ms:start_ms+1000,text,channel,meeting_id,is_final:true});
function start(h,id='B'){h.ctx.renderRecording({state:'recording',session_id:id,started_at_ms:1000});h.ctx.showLive();}
test('old meeting is immutable during live finals, partials, and row trimming; hidden finals are retained',()=>{
 const h=harness();start(h);const old=h.old(205),before=old.textContent;
 h.ctx.appendDeltas([d(0,17000),{...d(-1,18000),is_final:false},{...d(-1,18000,''),is_final:false}]);
 assert.equal(old.textContent,before);assert.equal(old.children.length,205);assert.equal(h.nodes.get('detail').scrollTop,10);
 h.ctx.showLive();assert.match(h.ctx.livePayload().text,/words/);
});
test('one speech origin, stable chronological copy, and original offsets survive late finals',()=>{
 const h=harness();start(h);h.ctx.appendDeltas([d(0,19000,'later'),d(1,17000,'first','mic'),d(2,36000,'last')]);
 const payload=h.ctx.livePayload();assert.match(payload.text,/\[00:00:00\] me: first/);assert.match(payload.text,/\[00:00:02\] them: later/);assert.match(payload.text,/\[00:00:19\] them: last/);assert.ok(payload.text.indexOf('first')<payload.text.indexOf('later'));
 assert.deepEqual(JSON.parse(h.run('JSON.stringify(liveSegments.map(s=>s.start_ms))')),[17000,19000,36000]);
 assert.match(h.document.getElementById('segments').textContent,/0:00mefirst/);
});
test('navigation does not reset the live buffer; stale sessions and repeated finals do not mix',()=>{
 const h=harness();start(h);h.ctx.appendDeltas([d(0,17000,'once'),d(0,17000,'once')]);h.old();h.ctx.appendDeltas([d(1,36000,'hidden')]);h.ctx.showLive();
 assert.equal((h.ctx.livePayload().text.match(/once/g)||[]).length,1);assert.match(h.ctx.livePayload().text,/hidden/);
 start(h,'C');h.ctx.appendDeltas([d(2,40000,'stale'),d(0,3000,'new','mic','C')]);assert.doesNotMatch(h.ctx.livePayload().text,/stale|hidden|once/);assert.match(h.ctx.livePayload().text,/\[00:00:00\] me: new/);
});
test('complete copy survives DOM cap and no mounted transcript; partials never establish zero',()=>{
 const h=harness();start(h);h.ctx.appendDeltas([{...d(-1,1000,'partial'),is_final:false}]);assert.doesNotMatch(h.ctx.livePayload().text,/partial/);
 h.ctx.clear(h.nodes.get('detail'));h.ctx.appendDeltas(Array.from({length:205},(_,i)=>d(i,17000+i*1000,'line'+i)));
 h.ctx.showLive();assert.equal(h.document.getElementById('segments').children.length,200);assert.match(h.ctx.livePayload().text,/\[00:00:00\] them: line0\n/);assert.match(h.ctx.livePayload().text,/line204/);
});
test('saved copy rebases both channels and leaves canonical input untouched',()=>{
 const h=harness(),segments=[d(0,36000,'later'),d(1,17000,'first','mic')];const before=JSON.stringify(segments);
 const payload=h.ctx.storedTranscriptPayload({meeting:{title:'Saved',started_at_ms:1000},segments});
 assert.match(payload.text,/\[00:00:00\] me: first/);assert.match(payload.text,/\[00:00:19\] them: later/);assert.equal(JSON.stringify(segments),before);
});

test('late saved-meeting responses cannot replace a newer selection or the live pane',async()=>{
 const h=harness();let finish;h.ctx.api=()=>new Promise(resolve=>finish=resolve);
 h.ctx.renderDetail=detail=>{h.nodes.get('detail').textContent=detail.meeting.id;};
 const pending=h.ctx.openMeeting('old');h.ctx.showLive();finish({meeting:{id:'old'}});await pending;
 assert.ok(h.document.getElementById('live-pane'));
});
test('only the matching persisted capture can replace its live view, including a late status poll',()=>{
 const h=harness();start(h);const opened=[];h.ctx.openMeeting=id=>opened.push(id);h.ctx.refreshLibrary=()=>{};
 h.ctx.handleStreamFrame({kind:'meeting_ready',reason:'persisted',session_id:'A',meeting_id:'saved-A'});
 h.ctx.renderRecording({state:'idle'});assert.deepEqual(opened,[]);
 h.ctx.handleStreamFrame({kind:'meeting_ready',reason:'persisted',session_id:'B',meeting_id:'saved-B'});assert.deepEqual(opened,['saved-B']);
 h.old();h.ctx.handleStreamFrame({kind:'meeting_ready',reason:'persisted',session_id:'B',meeting_id:'saved-B'});assert.equal(opened.length,1);
});
test('snapshot merges newer socket finals, retains full origin, and ignores responses from old sessions',async()=>{
 const h=harness();start(h);await Promise.resolve();let reply;h.ctx.api=()=>new Promise(resolve=>reply=resolve);
 const pending=h.ctx.recoverLiveHistory();h.ctx.appendDeltas([d(2,36000,'socket')]);
 reply({session_id:'B',deltas:[d(1,19000,'snapshot')],speech_origin_ms:17000,truncated:true});await pending;
 assert.match(h.ctx.livePayload().text,/\[00:00:02\] them: snapshot/);assert.match(h.ctx.livePayload().text,/\[00:00:19\] them: socket/);
 assert.match(h.document.getElementById('live-history').textContent,/unavailable/);
 const stale=h.ctx.recoverLiveHistory();const oldReply=reply;start(h,'C');oldReply({session_id:'B',deltas:[d(0,0,'stale')],speech_origin_ms:0,truncated:false});await stale;
 assert.doesNotMatch(h.ctx.livePayload().text,/stale/);
});
test('autoscroll follows only a reader already at the bottom',()=>{
 const h=harness();start(h);h.nodes.get('detail').scrollTop=900;h.ctx.appendDeltas([d(0,17000)]);assert.equal(h.nodes.get('detail').scrollTop,1000);
 h.nodes.get('detail').scrollTop=20;h.ctx.appendDeltas([d(1,20000)]);assert.equal(h.nodes.get('detail').scrollTop,20);
});
test('late replay finals do not retract a newer partial; partials do not commit an origin',()=>{
 const h=harness();start(h);h.ctx.appendDeltas([{...d(-1,40000,'still talking'),is_final:false},d(0,17000,'old final')]);
 assert.match(h.document.getElementById('segments').textContent,/still talking/);assert.doesNotMatch(h.ctx.livePayload().text,/still talking/);
 h.ctx.appendDeltas([{...d(-1,40000,''),is_final:false}]);assert.doesNotMatch(h.document.getElementById('segments').textContent,/still talking/);
});

test('empty transcripts and partials have no committed speech origin; ties remain stable',()=>{
 const h=harness();start(h);assert.equal(h.run('speechOrigin(liveSegments)'),null);
 h.ctx.appendDeltas([{...d(-1,1000,'uncommitted'),is_final:false}]);assert.match(h.document.getElementById('segments').textContent,/…/);
 h.ctx.appendDeltas([d(2,17000,'third'),d(0,17000,'first'),d(1,17000,'second')]);
 assert.ok(h.ctx.livePayload().text.indexOf('first')<h.ctx.livePayload().text.indexOf('second'));
 assert.ok(h.ctx.livePayload().text.indexOf('second')<h.ctx.livePayload().text.indexOf('third'));
});

test('saved handoff cannot steal a user navigation that is still loading',async()=>{
 const h=harness();start(h);let reply;h.ctx.api=()=>new Promise(resolve=>reply=resolve);h.ctx.refreshLibrary=()=>{};
 const pending=h.ctx.openMeeting('chosen-old');h.ctx.renderDetail=d=>{h.nodes.get('detail').textContent=d.meeting.id;};
 h.ctx.renderRecording({state:'idle'});h.ctx.handleStreamFrame({kind:'meeting_ready',reason:'persisted',session_id:'B',meeting_id:'saved-B'});
 reply({meeting:{id:'chosen-old'}});await pending;assert.equal(h.nodes.get('detail').textContent,'chosen-old');
});

test('UI and Rust exports share the same zero, leading-silence and multi-hour contract',()=>{
 const h=harness();
 for (const fixture of require('../fixtures/transcript-timing.json')) {
  const segments=fixture.offsets.map((ms,i)=>d(i,ms,'word'+i));
  assert.equal(h.ctx.speechOrigin(segments),fixture.origin);
  const payload=h.ctx.storedTranscriptPayload({meeting:{title:'Shared fixture',started_at_ms:1000},segments});
  fixture.labels.forEach(label=>{assert.ok(payload.text.includes('['+label+']'));assert.ok(payload.html.includes('['+label+']'));});
 }
});

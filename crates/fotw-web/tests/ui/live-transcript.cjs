const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function harness(notes) {
  class Element {
    constructor(tag) { this.tagName=tag; this.children=[]; this.listeners={}; this.scrollHeight=1000;this.clientHeight=100;this.scrollTop=10;this.dataset={};this.classList={add(){},toggle(){}}; }
    get firstChild(){return this.children[0];} get childElementCount(){return this.children.length;}
    get textContent(){return (this.ownText||'')+this.children.map(c=>c.textContent).join('');}
    set textContent(t){this.ownText=String(t);this.children=[];}
    appendChild(n){n.parent=this;this.children.push(n);return n;}
    insertBefore(n,ref){if(n.parent)n.parent.removeChild(n);const i=ref?this.children.indexOf(ref):this.children.length;this.children.splice(i,0,n);n.parent=this;return n;}
    removeChild(n){this.children.splice(this.children.indexOf(n),1);n.parent=null;}
    remove(){this.parent?.removeChild(this);}
    replaceWith(n){let p=this.parent,i=p.children.indexOf(this);p.children[i]=n;n.parent=p;}
    addEventListener(k,f){this.listeners[k]=f;} setAttribute(k,v){this[k]=v;}
  }
  const root=new Element('body');
  const nodes=new Map(['list','detail','status','record','live','elapsed','consent','consent-label','recording','recording-limit','return-live','capture-health','speech-health'].map(id=>{let n=root.appendChild(new Element('div'));n.id=id;return [id,n];}));
  const all=(n=root)=>[n,...n.children.flatMap(all)];
  const document={createElement:t=>new Element(t),getElementById:id=>all().find(n=>n.id===id)||null};
  const ctx=vm.createContext({FOTWNotes:notes,document,Date,Intl,Set,Map,URLSearchParams,window:{location:{search:''}},localStorage:{getItem:()=>null},setInterval:()=>1,clearInterval(){},setTimeout(){},XMLSerializer:class {serializeToString(n){return n.textContent;}}});
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


test('meeting selection is accessible, survives list/search refresh, and clears for live view',async()=>{
 const h=harness();
 h.ctx.api=async()=>({meeting:{id:'A'}});
 h.ctx.renderDetail=()=>{};
 const rows=[{id:'A',title:'First',started_at_ms:1000},{id:'B',title:'Second',started_at_ms:2000}];
 h.ctx.renderList(rows);
 await h.ctx.openMeeting('A');
 assert.equal(h.nodes.get('list').children[0]['aria-current'],'true');
 assert.equal(h.nodes.get('list').children[1]['aria-current'],'false');
 h.ctx.renderList(rows);
 assert.equal(h.nodes.get('list').children[0]['aria-current'],'true');
 h.ctx.renderHits([{meeting_id:'A',meeting_title:'First',source:'notes',snippet:'match'}]);
 assert.equal(h.nodes.get('list').children[0]['aria-current'],'true');
 h.ctx.showLive();
 assert.equal(h.nodes.get('list').children[0]['aria-current'],'false');
});

test('live notes mount once per session, keep their original clock, and link to the saved identity',()=>{
 const mounts=[],links=[];
 const notes={RecordingClock:require('../../ui/notes.js').RecordingClock,flushAll:()=>Promise.resolve(),
  mount:(id,parent,api,options)=>mounts.push({id,options}),finishSession:(...args)=>links.push(args)};
 const h=harness(notes);h.ctx.refreshLibrary=()=>{};
 h.ctx.renderRecording({state:'recording',session_id:'A',started_at_ms:1000,elapsed_ms:125});h.ctx.showLive();
 assert.equal(mounts.length,1);assert.equal(mounts[0].id,'A');assert.equal(mounts[0].options.session,true);
 assert.ok(mounts[0].options.clock()>=125);
 h.ctx.renderRecording({state:'recording',session_id:'A',started_at_ms:1000,elapsed_ms:200});
 assert.equal(mounts.length,1,'status polling must not replace the focused editor');
 h.ctx.renderRecording({state:'recording',session_id:'B',started_at_ms:9000,elapsed_ms:0});
 assert.equal(mounts.length,2);assert.equal(mounts[0].options.clock(),null,'old draft cannot use the new capture clock');
 h.ctx.handleStreamFrame({kind:'meeting_ready',reason:'persisted',session_id:'B',meeting_id:'saved-B'});
 assert.deepEqual(links.at(-1),['B','saved-B']);
});

test('recovery navigation opens the selected draft and a late saved alias cannot steal a newer view',async()=>{
 let selected,alias;const recovered=[],opened=[];
 const notes={RecordingClock:require('../../ui/notes.js').RecordingClock,flushAll:()=>Promise.resolve(),mount(){},finishSession(){},
  mountRecovery:(parent,api,onOpen)=>{selected=onOpen;},mountRecovered:(id,parent,api,options)=>{recovered.push(id);alias=options.onSaved;}};
 const h=harness(notes);h.ctx.openMeeting=id=>opened.push(id);
 await h.ctx.showNoteDrafts();assert.equal(typeof selected,'function');selected('orphan-A');
 assert.deepEqual(recovered,['orphan-A']);h.ctx.showLive();alias('late-saved');assert.deepEqual(opened,[]);
 h.ctx.renderRecording({state:'recording',session_id:'live-A',started_at_ms:1000,elapsed_ms:100});
 h.ctx.showRecoveredNote('live-A');assert.ok(h.document.getElementById('live-pane'));
 assert.equal(recovered.length,1,'active draft returns to the live editor');
});

test('capture health distinguishes a healthy system leg from an exhausted microphone',()=>{
 const h=harness();
 h.ctx.renderRecording({state:'recording',session_id:'fixture-session',started_at_ms:1000,
 capture_health:{system:{state:'healthy',attempts:0,max_attempts:3},mic:{state:'exhausted',attempts:3,max_attempts:3}}});
 const panel=h.nodes.get('capture-health');
 assert.match(panel.textContent,/System audio: Healthy/);
 assert.match(panel.textContent,/Microphone: Automatic recovery stopped/);
 const retry=h.all(panel).find(n=>n.tagName==='button'&&!n.hidden);
 assert.ok(retry,'exhausted source needs a manual retry');
 assert.equal(retry.textContent,'Retry microphone');
 h.ctx.renderRecording({state:'finishing',session_id:'fixture-session',elapsed_ms:1000});
 assert.equal(panel.hidden,true);
 assert.equal(panel.textContent,'');
});

test('recovery polling preserves controls and retry sends the exact session and source once',async()=>{
 const h=harness(); const body={state:'recording',session_id:'fixture / A',started_at_ms:1000,
 capture_health:{system:{state:'healthy',attempts:0,max_attempts:3},mic:{state:'exhausted',attempts:3,max_attempts:3}}};
 h.ctx.renderRecording(body);const panel=h.nodes.get('capture-health');
 const button=h.all(panel).find(n=>n.tagName==='button'&&!n.hidden);
 let reply,calls=[];h.ctx.api=(url,opts)=>{calls.push([url,opts.method]);return new Promise(resolve=>reply=resolve);};
 const pending=button.listeners.click();assert.equal(button.disabled,true);
 h.ctx.renderRecording(body);
 assert.equal(h.all(panel).find(n=>n.tagName==='button'&&!n.hidden),button,'polling must preserve focusable elements');
 assert.equal(button.disabled,true);await button.listeners.click();assert.equal(calls.length,1);
 assert.deepEqual(calls,[['/api/recording/retry?session_id=fixture%20%2F%20A&source=mic','POST']]);
 reply({...body,capture_health:{...body.capture_health,mic:{state:'recovering',attempts:0,max_attempts:3,retry_in_ms:12000}}});await pending;
 assert.match(panel.textContent,/Recovering audio/);assert.match(panel.textContent,/next attempt in 12s/);
 assert.equal(button.hidden,true);
});

test('late recovery replies cannot restore a stopped or replaced recording',async()=>{
 const h=harness(); const body={state:'recording',session_id:'old',started_at_ms:1000,
 capture_health:{system:{state:'healthy'},mic:{state:'exhausted',attempts:3,max_attempts:3}}};
 h.ctx.renderRecording(body);const button=h.all(h.nodes.get('capture-health')).find(n=>n.tagName==='button'&&!n.hidden);
 let reply;h.ctx.api=url=>url.startsWith('/api/recording/retry') ? new Promise(resolve=>reply=resolve) : Promise.resolve({session_id:'new',deltas:[]});
 const pending=button.listeners.click();
 h.ctx.renderRecording({state:'finishing',session_id:'old',elapsed_ms:1000});
 h.ctx.renderRecording({...body,session_id:'new',capture_health:{system:{state:'healthy'},mic:{state:'confirming'}}});
 reply(body);await pending;
 assert.equal(h.run('liveSessionId'),'new');
 assert.match(h.nodes.get('capture-health').textContent,/Microphone: Checking audio delivery/);
 assert.equal(h.all(h.nodes.get('capture-health')).some(n=>n.tagName==='button'&&!n.hidden),false);
});

test('Stop disables capture retry immediately even before its response arrives',async()=>{
 const h=harness();const body={state:'recording',session_id:'fixture',started_at_ms:1000,
 capture_health:{system:{state:'healthy'},mic:{state:'exhausted'}}};
 h.ctx.renderRecording(body);const panel=h.nodes.get('capture-health');
 const retry=h.all(panel).find(n=>n.tagName==='button'&&!n.hidden);
 let resolveStop,calls=[];h.ctx.api=(url)=>{calls.push(url);return new Promise(resolve=>resolveStop=resolve);};
 const stopped=h.ctx.onRecord();
 assert.equal(panel.hidden,true,'a pending Stop must remove the retry action');
 h.ctx.renderRecording(body);assert.equal(panel.hidden,true,'a stale status poll cannot re-enable retry during Stop');
 assert.equal(h.nodes.get('record').disabled,true);await h.ctx.onRecord();
 await retry.listeners.click();assert.deepEqual(calls,['/api/recording/stop']);
 resolveStop({state:'finishing',session_id:'fixture',elapsed_ms:1000});await stopped;
 assert.equal(panel.hidden,true);
});

test('a status poll cannot swallow the current session retry error',async()=>{
 const h=harness(),messages=[];h.ctx.say=message=>messages.push(message);
 const body={state:'recording',session_id:'retry-error',started_at_ms:1000,
 capture_health:{system:{state:'healthy'},mic:{state:'exhausted',attempts:3,max_attempts:3}}};
 h.ctx.renderRecording(body);
 const button=h.all(h.nodes.get('capture-health')).find(n=>n.tagName==='button'&&!n.hidden);
 let reply;h.ctx.api=url=>url.startsWith('/api/recording/retry') ? new Promise(resolve=>reply=resolve) : Promise.resolve(body);
 const pending=button.listeners.click();
 await h.ctx.pollRecording();
 messages.length=0; // Ignore the harness's unrelated unauthorised bootstrap status.
 reply({...body,error:'Microphone retry was refused'});await pending;
 assert.deepEqual(messages,['Microphone retry was refused']);
 assert.equal(button.disabled,false,'a refused retry must remain available');
});

test('an accepted queued retry stays disabled through exhausted snapshots until the pump advances',async()=>{
 const h=harness();const body={state:'recording',session_id:'retry-queued',started_at_ms:1000,
 capture_health:{system:{state:'healthy'},mic:{state:'exhausted',attempts:3,max_attempts:3}}};
 h.ctx.renderRecording(body);const panel=h.nodes.get('capture-health');
 const button=h.all(panel).find(n=>n.tagName==='button'&&!n.hidden);
 h.ctx.api=async()=>body;
 await button.listeners.click();
 assert.equal(button.disabled,true,'POST acknowledgment is not pump acknowledgment');
 h.ctx.renderRecording(body);assert.equal(button.disabled,true,'old exhausted status must not re-enable Retry');
 h.ctx.renderRecording({...body,capture_health:{system:{state:'healthy'},mic:{state:'recovering',attempts:0,max_attempts:3}}});
 assert.equal(button.hidden,true);
 h.ctx.renderRecording(body);
 assert.equal(button.disabled,false,'a later exhausted episode may be retried again');
});

test('a pump transition observed before the POST reply releases the retry for a later episode',async()=>{
 const h=harness();const body={state:'recording',session_id:'retry-poll-first',started_at_ms:1000,
 capture_health:{system:{state:'healthy'},mic:{state:'exhausted',attempts:3,max_attempts:3}}};
 const recovering={...body,capture_health:{system:{state:'healthy'},mic:{state:'recovering',attempts:0,max_attempts:3}}};
 h.ctx.renderRecording(body);const panel=h.nodes.get('capture-health');
 const button=h.all(panel).find(n=>n.tagName==='button'&&!n.hidden);
 let reply;h.ctx.api=url=>url.startsWith('/api/recording/retry') ? new Promise(resolve=>reply=resolve) : Promise.resolve(recovering);
 const pending=button.listeners.click();await h.ctx.pollRecording();
 reply(body);await pending;
 assert.match(panel.textContent,/Recovering audio/,'the old POST snapshot cannot rewind a newer poll');
 h.ctx.renderRecording(body);
 assert.equal(button.disabled,false,'the pump already acknowledged the previous retry');
});


test('incomplete native cleanup explains restart and offers no retry',()=>{
 const h=harness();
 h.ctx.renderRecording({state:'recording',session_id:'fixture-session',elapsed_ms:1000,
 capture_health:{system:{state:'healthy'},mic:{state:'cleanup_blocked'}}});
 const panel=h.nodes.get('capture-health');
 assert.match(panel.textContent,/System audio: Healthy/);
 assert.match(panel.textContent,/Microphone: Audio cleanup incomplete; reopen FlyOnTheWall/);
 assert.equal(h.all(panel).some(n=>n.tagName==='button'&&!n.hidden),false);
});

test('preparation has no recording clock or red badge and Cancel does not require another consent tick',async()=>{
 const h=harness();
 h.ctx.renderRecording({state:'preparing',started_at_ms:1000,elapsed_ms:500});
 assert.equal(h.nodes.get('recording').hidden,false);
 assert.equal(h.nodes.get('recording').textContent,'preparing…');
 assert.equal(h.nodes.get('recording').className,'preparing');
 assert.equal(h.nodes.get('elapsed').hidden,true);
 assert.equal(h.nodes.get('record').textContent,'Cancel');
 assert.equal(h.nodes.get('record').disabled,false);
 const called=[];
 h.ctx.api=async path=>{called.push(path);return {state:'preparing',preparation_cancel_requested:true};};
 await h.ctx.onRecord();
 assert.equal(called[0],'/api/recording/stop');
 assert.ok(!called.some(path=>path.includes('/start')));
 assert.equal(h.nodes.get('record').disabled,true);
});

test('a late Start reply cannot replace the cancellation of owned preparation',async()=>{
 const h=harness();h.nodes.get('consent').checked=true;
 let reply, starts=0;
 h.ctx.api=path=>path.includes('/start') && starts++===0?new Promise(resolve=>{reply=resolve;}):Promise.resolve({state:'preparing',preparation_cancel_requested:true});
 const start=h.ctx.onRecord();
 h.ctx.renderRecording({state:'preparing'});
 await h.ctx.onRecord();
 reply({state:'recording',session_id:'late-source',started_at_ms:1000});
 await start;
 assert.equal(h.nodes.get('recording').className,'preparing');
 assert.equal(h.nodes.get('record').disabled,true);
 assert.equal(h.nodes.get('elapsed').hidden,true);
});

test('capture setup offers Stop and never claims that audio has not started',async()=>{
 const h=harness();const paths=[];
 h.ctx.renderRecording({state:'starting',preparation_cancel_requested:false});
 assert.equal(h.nodes.get('record').textContent,'Stop');
 assert.equal(h.nodes.get('recording').textContent,'starting capture…');
 h.ctx.api=async p=>{paths.push(p);return {state:'starting',preparation_cancel_requested:true};};
 await h.ctx.onRecord();
 assert.equal(paths[0],'/api/recording/stop');
 assert.match(h.nodes.get('status').textContent,/Stopping capture/);
 assert.doesNotMatch(h.nodes.get('status').textContent,/Cancelled|Audio has not started/);
});

test('pending Start polls setup every second so Cancel is promptly available',async()=>{
 const h=harness(),intervals=[];let release;
 h.ctx.setInterval=(_,ms)=>{intervals.push(ms);return 1;};
 h.nodes.get('consent').checked=true;
 h.ctx.api=()=>new Promise(resolve=>{release=resolve;});
 const pending=h.ctx.onRecord();
 assert.ok(intervals.includes(1000));
 h.ctx.renderRecording({state:'preparing'});
 assert.equal(h.nodes.get('record').textContent,'Cancel');
 assert.equal(h.nodes.get('record').disabled,false);
 release({state:'preparing'});await pending;
});

test('setup diagnostics never claim a recording or transcription failure before capture readiness',()=>{
 const h=harness();
 h.ctx.renderRecording({state:'starting',transcription_error:'Stop requested; any buffered audio will be saved'});
 assert.match(h.nodes.get('status').textContent,/^Capture setup:/);
 h.ctx.renderRecording({state:'preparing',transcription_error:'Model unavailable'});
 assert.match(h.nodes.get('status').textContent,/^Preparation:/);
 assert.doesNotMatch(h.nodes.get('status').textContent,/Recording, but/);
});

const speechStatus=(overrides={})=>({source:'system',provider:'deepgram',model:'nova-3',revision:null,state:'cloud',epoch:0,from_ms:0,switch_reason:null,pending_audio_ms:3200,word_timestamps_supported:true,diarization_supported:true,worker_running:true,error:null,...overrides});
test('live source status follows the actual fallback engine, original clock and pending coverage',()=>{
 const h=harness();
 h.ctx.renderRecording({state:'recording',session_id:'engine-A',started_at_ms:1000,speech_status:[speechStatus()]});
 const panel=h.nodes.get('speech-health');assert.equal(panel.hidden,false);
 assert.match(panel.textContent,/System audio.*deepgram.*nova-3/i);assert.match(panel.textContent,/3.2s.*pending/i);
 h.ctx.renderRecording({state:'recording',session_id:'engine-A',started_at_ms:1000,speech_status:[speechStatus({provider:'whispercpp',model:'small.en',state:'local',epoch:1,from_ms:17000,switch_reason:'transport_budget',word_timestamps_supported:false,diarization_supported:false})]});
 assert.match(panel.textContent,/small.en/);assert.doesNotMatch(panel.textContent,/nova-3/);
 assert.match(panel.textContent,/network retry budget/i);assert.match(panel.textContent,/audio.*17/);
 assert.match(panel.textContent,/no remote speaker labels/i);
});
test('incomplete native work remains visible after Stop and disappears on Idle',()=>{
 const h=harness();h.ctx.refreshLibrary=()=>{};
 h.ctx.renderRecording({state:'finishing',session_id:'engine-A',started_at_ms:1000,ended_at_ms:41000,elapsed_ms:40000,speech_status:[speechStatus({provider:'whispercpp',model:'small.en',state:'incomplete',worker_running:true,error:'inference timed out'})]});
 const panel=h.nodes.get('speech-health');assert.equal(panel.hidden,false);assert.match(panel.textContent,/incomplete/i);assert.match(panel.textContent,/Start waits for cleanup/i);
 h.ctx.renderRecording({state:'idle',speech_status:[speechStatus()]});assert.equal(panel.hidden,true);assert.equal(panel.textContent,'');
});
test('legacy unknown coverage never renders as zero pending or confirmed complete, and model text is inert',()=>{
 const h=harness();h.ctx.renderRecording({state:'recording',session_id:'legacy',started_at_ms:1000,speech_status:[speechStatus({model:'<img src=x>',state:'closed',pending_audio_ms:null,epoch:null,from_ms:null,worker_running:null})]});
 const panel=h.nodes.get('speech-health');assert.match(panel.textContent,/coverage.*not measured/i);assert.doesNotMatch(panel.textContent,/0s.*pending|transcription complete/i);
 assert.match(panel.textContent,/<img src=x>/);assert.ok(h.all(panel).every(node=>node.tagName!=='img'));
});

test('source details stay expanded and focused controls are preserved across status polls',()=>{
 const h=harness();
 const body={state:'recording',session_id:'engine-A',started_at_ms:1000,speech_status:[speechStatus()]};
 h.ctx.renderRecording(body);
 const details=h.all(h.nodes.get('speech-health')).find(node=>node.tagName==='details');
 details.open=true;
 h.ctx.renderRecording({...body,speech_status:[speechStatus({pending_audio_ms:700})]});
 const updated=h.all(h.nodes.get('speech-health')).find(node=>node.tagName==='details');
 assert.equal(updated,details);assert.equal(updated.open,true);
});

test('the actual whisper.cpp provider ID renders the friendly engine name',()=>{
 const h=harness();h.ctx.renderRecording({state:'recording',session_id:'real-provider',started_at_ms:1000,speech_status:[speechStatus({provider:'whispercpp',model:'small.en',state:'local'})]});
 const panel=h.nodes.get('speech-health');assert.match(panel.textContent,/Whisper \/ small.en/);assert.doesNotMatch(panel.textContent,/whispercpp/);
});
test('legacy failures retain their warning while a durable epoch zero uses a recovery notice',()=>{
 const h=harness();const base={state:'recording',session_id:'errors',started_at_ms:1000,transcription_error:'The API key was rejected'};
 h.ctx.renderRecording({...base,speech_status:[speechStatus({epoch:null,from_ms:null,pending_audio_ms:null,state:'closed',worker_running:null})]});
 assert.match(h.nodes.get('status').textContent,/Recording, but transcription is failing:/);
 h.ctx.renderRecording({...base,speech_status:[speechStatus({epoch:0})]});
 assert.match(h.nodes.get('status').textContent,/^Recording notice:/);
});


test('saved meetings retain actual source engine boundaries on the recording clock',()=>{
 const h=harness();h.ctx.actionsRow=()=>null;h.ctx.mountGithubSync=()=>{};h.ctx.mountSharingDocument=()=>{};
 h.ctx.renderDetail({meeting:{id:'saved',title:'Synthetic',started_at_ms:1000},segments:[],speech_provenance:[
 {source:'system',epoch:0,from_sample:0,through_sample:272000,provider:'deepgram',model:'nova-3',revision:null,reason:null,capabilities:{diarization:'both',wordTimestamps:'both'}},
 {source:'system',epoch:1,from_sample:272000,through_sample:480000,provider:'whispercpp',model:'small.en',revision:'fixture-sha',reason:'transport_budget',capabilities:{diarization:'none',wordTimestamps:'none'}}]});
 const content=h.nodes.get('detail').textContent;
 assert.match(content,/Transcription sources/);assert.match(content,/Deepgram \/ nova-3/);assert.match(content,/Whisper \/ small.en/);
 assert.match(content,/0:17/);assert.match(content,/0:30/);assert.match(content,/network retry budget/i);assert.match(content,/Speaker labels unavailable/);
 assert.match(content,/original recording clock/i);assert.doesNotMatch(content,/All audio transcribed/);
});
test('old meeting details explicitly preserve unknown engine history',()=>{
 const h=harness();h.ctx.actionsRow=()=>null;h.ctx.mountGithubSync=()=>{};h.ctx.mountSharingDocument=()=>{};
 h.ctx.renderDetail({meeting:{id:'old',title:'Synthetic',started_at_ms:1000},segments:[]});
 assert.match(h.nodes.get('detail').textContent,/Engine history was not recorded/);
});


test('unconfirmed saved epochs never claim processed audio and metadata remains text',()=>{
 const h=harness();const panel=h.ctx.renderSavedSpeechProvenance([{source:'system',epoch:0,from_sample:16000,through_sample:null,provider:'constructor',model:'<img src=x onerror=alert(1)>',revision:null,reason:null,capabilities:{}}]);
 assert.match(panel.textContent,/no confirmed processed audio/);assert.doesNotMatch(panel.textContent,/Audio processed/);
 assert.match(panel.textContent,/constructor \/ <img/);assert.equal(h.all(panel).some(n=>n.tagName==='img'),false);
});

test('closed local-only streams explain that native cleanup still blocks Start',()=>{
 const h=harness();h.ctx.refreshLibrary=()=>{};
 h.ctx.renderRecording({state:'finishing',session_id:'legacy-local',started_at_ms:1000,ended_at_ms:2000,speech_status:[speechStatus({provider:'whispercpp',model:'small.en',state:'closed',epoch:null,from_ms:null,pending_audio_ms:null,worker_running:true})]});
 const panel=h.nodes.get('speech-health');assert.match(panel.textContent,/Start waits for cleanup/);assert.match(panel.textContent,/Coverage not measured/);
 h.ctx.renderRecording({state:'idle'});assert.equal(panel.hidden,true);
});


test('partial updates retain committed rows and do bounded rendering work with long history',()=>{
 const h=harness();start(h);
 h.ctx.appendDeltas(Array.from({length:10000},(_,i)=>d(i,17000+i*1000,'line'+i)));
 h.ctx.appendDeltas([{...d(-1,10018000,'first partial'),is_final:false}]);
 const body=h.document.getElementById('segments'),first=body.children[0];
 let rows=0,sortedSamples=0;const render=h.ctx.segmentRow,order=h.ctx.orderedSegments;
 h.ctx.segmentRow=(...args)=>{rows++;return render(...args);};
 h.ctx.orderedSegments=segments=>{sortedSamples+=segments.length;return order(segments);};
 h.nodes.get('detail').scrollTop=20;
 for(let i=0;i<40;i++)h.ctx.appendDeltas([{...d(-1,10018000,'partial '+i),is_final:false}]);
 assert.equal(body.children[0],first,'stable committed speech must retain its node');
 assert.equal(body.children.length,200);
 assert.equal(rows,40,'only the changing partial should be rendered');
 assert.equal(sortedSamples,0,'partial updates must not rescan/sort committed history');
 assert.equal(h.nodes.get('detail').scrollTop,20,'reading position must be retained');
 assert.match(body.textContent,/partial 39/);
 assert.match(h.ctx.livePayload().text,/line0\n/,'copy still includes offscreen history');
 assert.doesNotMatch(h.ctx.livePayload().text,/partial 39/);
});

const test=require('node:test'),assert=require('node:assert/strict');
const {spawnSync}=require('node:child_process');
const {fixture,settle}=require('../fixtures/notes-lifecycle.cjs');

test('reopening clean saved notes fetches the current server text and revision',async()=>{
  const h=fixture();let reads=0,server={revision:1,body_md:'first saved text',anchors:[]};
  const api=async()=>{reads++;return structuredClone(server);};
  const firstHost=h.host();h.notes.mount('meeting-A',firstHost,api);await settle();
  await h.notes.release(firstHost);firstHost.children=[];
  server={revision:2,body_md:'updated by another tab',anchors:[]};
  const reopened=h.notes.mount('meeting-A',h.host(),api);await settle();
  assert.equal(reopened.body,server.body_md);
  assert.equal(reopened.revision,2);
  assert.equal(reads,2);
});

test('failed offscreen saves retain the exact draft and unload warning for retry',async()=>{
  const h=fixture();let fail=true,reads=0,server={revision:1,body_md:'saved',anchors:[]};
  const api=async(_,options)=>{
    if(!options){reads++;return structuredClone(server);}
    if(fail)throw new Error('fixture offline');
    server={...JSON.parse(options.body),revision:2};return structuredClone(server);
  };
  const parent=h.host(),draft=h.notes.mount('meeting-B',parent,api);await settle();
  draft.edit('unsaved 日本語\nsecond line');await h.notes.release(parent);parent.children=[];
  const reopened=h.notes.mount('meeting-B',h.host(),api);await settle();
  assert.equal(reopened.body,'unsaved 日本語\nsecond line');
  assert.equal(reopened.status,'error');assert.equal(reopened.dirty,true);assert.equal(reads,1);
  let warned=false;h.events.get('beforeunload')({preventDefault(){warned=true;}});
  assert.equal(warned,true);
  fail=false;await reopened.retrySave();assert.equal(reopened.dirty,false);
  assert.equal(server.body_md,'unsaved 日本語\nsecond line');
});

test('direct remount keeps the active editor registered for unsaved warnings',async()=>{
  const h=fixture(),api=async()=>({revision:1,body_md:'saved',anchors:[]});
  h.notes.mount('meeting-C',h.host(),api);await settle();
  const remounted=h.notes.mount('meeting-C',h.host(),api);await settle();
  remounted.edit('new unsaved text');
  let warned=false;h.events.get('beforeunload')({preventDefault(){warned=true;}});
  assert.equal(warned,true);
});

test('a slow earlier load cannot replace a newer remount response',async()=>{
  const h=fixture();let reads=0,finishOld;
  const api=async()=>{
    if(++reads===1)return new Promise(resolve=>finishOld=resolve);
    return {revision:2,body_md:'newer saved notes',anchors:[]};
  };
  const firstHost=h.host();h.notes.mount('meeting-D',firstHost,api);await settle();
  await h.notes.release(firstHost);firstHost.children=[];
  const reopened=h.notes.mount('meeting-D',h.host(),api);await settle();
  assert.equal(reopened.body,'newer saved notes');
  finishOld({revision:1,body_md:'older read snapshot',anchors:[]});await settle();
  assert.equal(reopened.body,'newer saved notes');assert.equal(reopened.revision,2);
  assert.equal(reopened.status,'saved');
});

test('an earlier read error cannot replace the newer saved state',async()=>{
  const h=fixture();let reads=0,failOld;
  const api=async()=>++reads===1?new Promise((_,reject)=>failOld=reject):{revision:2,body_md:'newer',anchors:[]};
  h.notes.mount('meeting-E',h.host(),api);await settle();
  const reopened=h.notes.mount('meeting-E',h.host(),api);await settle();
  failOld(new Error('old request failed'));await settle();
  assert.equal(reopened.body,'newer');assert.equal(reopened.status,'saved');
});

test('a stale live read cannot shorten finalized bounds or remove saved anchors',async()=>{
  const h=fixture();let reads=0,finishOld;
  const anchor={id:'original-block',block_idx:0,block_text:'final notes',typed_at_ms:800};
  const api=async()=>++reads===1?new Promise(resolve=>finishOld=resolve):{
    meeting_id:'final-meeting',elapsed_ms:1000,note:{revision:2,body_md:'final notes',anchors:[anchor]},
  };
  const options={session:true,clock:()=>900,newId:()=> 'unused'};
  h.notes.mount('session-A',h.host(),api,options);await settle();
  const reopened=h.notes.mount('session-A',h.host(),api,options);await settle();
  finishOld({meeting_id:null,elapsed_ms:100,note:{revision:1,body_md:'old live text',anchors:[]}});await settle();
  assert.equal(reopened.body,'final notes');assert.equal(reopened.captureElapsed,1000);
  assert.equal(reopened.anchors[0].id,'original-block');assert.equal(reopened.anchors[0].typed_at_ms,800);
  assert.equal(reopened.dirty,false);
});

test('clean session and saved-meeting aliases both refresh without losing first input time',async()=>{
  const h=fixture();let revision=0,server={revision,body_md:'',anchors:[]};
  const api=async(path,options)=>{
    if(options)server={...JSON.parse(options.body),revision:++revision};
    return path.includes('/sessions/')?{meeting_id:null,elapsed_ms:1000,note:structuredClone(server)}:structuredClone(server);
  };
  const host=h.host(),draft=h.notes.mount('session-B',host,api,{session:true,clock:()=>125,newId:()=> 'first-block'});await settle();
  draft.edit('during recording');await draft.flush();
  await h.notes.finishSession('session-B','saved-B');await h.notes.release(host);host.children=[];
  server={revision:2,body_md:'updated saved notes',anchors:[{...server.anchors[0],block_text:'updated saved notes'}]};
  const reopened=h.notes.mount('saved-B',h.host(),api);await settle();
  assert.equal(reopened.body,'updated saved notes');assert.equal(reopened.revision,2);
  assert.equal(reopened.anchors[0].id,'first-block');assert.equal(reopened.anchors[0].typed_at_ms,125);
});

test('clean inactive note drafts are collectible while the page remains loaded',()=>{
  const source=`
    const {fixture,settle}=require(process.argv[1]);
    (async()=>{
      const h=fixture(),refs=[],api=async(path)=>{
        const note={revision:1,body_md:'saved',anchors:[]};
        return path.includes('/sessions/')?{note,meeting_id:null}:note;
      };
      for(let i=0;i<20;i++){
        let parent=h.host(),draft=h.notes.mount('entry-'+i,parent,api,{session:i%2===0});await settle();
        if(!draft.loaded)throw new Error('Fixture did not load');
        if(i%2===0)h.notes.linkSession('entry-'+i,'saved-'+i);
        refs.push(new WeakRef(draft));await h.notes.release(parent);
        parent.children=[];parent=null;draft=null;
      }
      for(let i=0;i<8;i++){await new Promise(setImmediate);global.gc();}
      console.log(JSON.stringify({retained:refs.filter(ref=>ref.deref()).length,pageLoaded:typeof h.notes.mount==='function'}));
    })().catch(error=>{console.error(error);process.exitCode=1;});
  `;
  const run=spawnSync(process.execPath,['--expose-gc','-e',source,require.resolve('../fixtures/notes-lifecycle.cjs')],{encoding:'utf8',timeout:15000});
  assert.equal(run.status,0,run.stderr);
  const result=JSON.parse(run.stdout);assert.equal(result.pageLoaded,true);
  assert.equal(result.retained,0,'clean drafts must not accumulate in the page cache');
});

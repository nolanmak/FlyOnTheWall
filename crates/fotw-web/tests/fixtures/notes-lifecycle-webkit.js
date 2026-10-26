// Invoke runNotesLifecycleFixture() in the owned nonpersistent WebKit runner
// against ui_preview's throwaway encrypted library. No devices or providers.
async function runNotesLifecycleFixture() {
  const pause=ms=>new Promise(resolve=>setTimeout(resolve,ms));
  async function until(check) {
    for(let i=0;i<300;i++){if(await check())return;await pause(10);}
    throw Error('Notes fixture condition timed out');
  }
  await until(()=>typeof token==='function' && token());
  const meetings=(await api('/api/meetings')).meetings.filter(m=>m.state==='ready');
  if(meetings.length<2)throw Error('Two synthetic meetings required');
  const [a,b]=meetings,realApi=api;
  const path='/api/meetings/'+encodeURIComponent(a.id)+'/notes';
  let failWrites=false,refusedWrites=0,reads=0,holdRead=false,oldCaptured=false,releaseOld;
  api=async(url,options)=>{
    if(url===path){
      if(options?.method==='POST' && failWrites){refusedWrites++;throw Error('Synthetic offline write');}
      if(!options || options.method!=='POST'){
        reads++;
        if(holdRead){
          holdRead=false;const snapshot=await realApi(url,options);oldCaptured=true;
          await new Promise(resolve=>releaseOld=resolve);return snapshot;
        }
      }
    }
    return realApi(url,options);
  };
  const field=()=>document.querySelector('.notes.note-editor textarea');
  const status=()=>document.querySelector('.notes.note-editor [role="status"]');
  async function open(id){await openMeeting(id);await until(()=>field()?.disabled===false);}
  async function write(body){
    const current=await realApi(path);
    return realApi(path,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({
      revision:current.revision,body_md:body,
      anchors:[{id:'native-note-block',block_idx:0,block_text:body,typed_at_ms:500}],
    })});
  }
  try {
    await write('Synthetic saved notes α');
    await open(b.id);await open(a.id);
    if(field().value!=='Synthetic saved notes α')throw Error('Initial notes not displayed');
    await open(b.id);
    await write('Updated from another tab 日本語');
    await open(a.id);
    if(field().value!=='Updated from another tab 日本語')throw Error('Clean remount displayed stale notes');

    // Leave a real HTTP read snapshot pending across another detail remount.
    await open(b.id);holdRead=true;
    await openMeeting(a.id);await until(()=>oldCaptured);
    await write('Newer overlapping-read snapshot 😀');
    await open(a.id);releaseOld();await pause(50);
    if(field().value!=='Newer overlapping-read snapshot 😀' || status().textContent!=='Saved')
      throw Error('Earlier read replaced the latest browser state');

    const unsaved=field().value+' — pending 日本語\n\nSecond line 😀';
    failWrites=true;field().value=unsaved;
    field().dispatchEvent(new Event('input',{bubbles:true}));
    await open(b.id);await until(()=>refusedWrites>0);await open(a.id);
    if(field().value!==unsaved || !status().textContent.includes('Could not save'))
      throw Error('Offscreen failed save lost its draft/error');
    const warning=new Event('beforeunload',{cancelable:true});window.dispatchEvent(warning);
    if(!warning.defaultPrevented)throw Error('Unsaved notes did not warn');
    if((await realApi(path)).body_md===unsaved)throw Error('Synthetic refused write changed saved notes');
    failWrites=false;
    const retry=[...document.querySelectorAll('.notes.note-editor button')].find(button=>button.textContent==='Save / retry');
    if(!retry)throw Error('Notes retry action missing');
    retry.click();await until(()=>status().textContent==='Saved');
    const saved=await realApi(path);
    if(saved.body_md!==unsaved || saved.anchors[0]?.id!=='native-note-block' || saved.anchors[0]?.typed_at_ms!==500)
      throw Error('Retry changed text or original anchor identity/time');
    for(let i=0;i<20;i++)await open(i%2?a.id:b.id);
    await open(a.id);
    if(field().value!==unsaved || reads<12)throw Error('Clean browsing did not refresh saved notes');
    const cleanWarning=new Event('beforeunload',{cancelable:true});window.dispatchEvent(cleanWarning);
    if(cleanWarning.defaultPrevented)throw Error('Acknowledged notes still warn as unsaved');
    const scriptHashes={};
    for(const file of ['app.js','notes.js']){
      const source=await (await fetch('/assets/'+file)).text();
      const digest=await crypto.subtle.digest('SHA-256',new TextEncoder().encode(source));
      scriptHashes[file]=[...new Uint8Array(digest)].map(n=>n.toString(16).padStart(2,'0')).join('');
    }
    field().scrollIntoView({block:'center'});
    return {cssWidth:innerWidth,cleanNotesRefreshed:true,olderReadIgnored:true,
      failedDraftPreserved:true,retrySaved:true,anchorIdentityAndTimePreserved:true,
      cleanNavigationCount:20,notesReads:reads,refusedFixtureWrites:refusedWrites,
      realEncryptedBackend:true,inputMechanism:'owned DOM events',scriptHashes,
      providerCalls:0,captureDevicesUsed:false,clipboardUsed:false,
      processMemoryMeasured:false,browserGcForced:false};
  } finally {api=realApi;}
}

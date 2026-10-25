// Saved-audio recovery. Read-only until an explicit retry action.
globalThis.FOTWSpeechRecovery = { mount(id, parent, api, reload) {
  const make=(tag,value)=>{const n=document.createElement(tag);n.textContent=value;return n;};
  const root=document.createElement('section');root.className='speech-recovery';root.hidden=true;
  const heading=root.appendChild(make('h3','Finish transcription'));
  const message=root.appendChild(make('p','Checking saved speech…'));
  message.setAttribute('role','status');message.setAttribute('aria-live','polite');
  const sources=root.appendChild(document.createElement('div'));sources.className='recovery-sources';
  const explanation=root.appendChild(make('p','Recover saved speech on this Mac using the original model. Your recording settings stay the same.'));explanation.className='meta';
  const actions=root.appendChild(document.createElement('div'));actions.className='recovery-actions';
  const retry=actions.appendChild(make('button','Retry locally'));
  const cancel=actions.appendChild(make('button','Cancel retry'));
  const refresh=actions.appendChild(make('button','Refresh status'));
  const reloadButton=actions.appendChild(make('button','Reload transcript'));
  for (const button of [retry,cancel,refresh,reloadButton]) button.type='button';
  retry.disabled=true;cancel.hidden=true;reloadButton.hidden=true;
  parent.appendChild(root);
  const base='/api/meetings/'+encodeURIComponent(id)+'/speech-recovery';
  let request=0, timer=false, hasRun=false;
  const current=seq=>root.isConnected!==false&&seq===request;
  function poll() {
    if (timer||root.isConnected===false) return;
    timer=true;
    setTimeout(async()=>{timer=false;if(root.isConnected!==false) await load();},1000);
  }
  function render(state) {
    root.hidden=false;
    const running=state.worker_running===true;
    hasRun=hasRun||running;
    heading.textContent=state.phase==='partial'?'Saved speech recovered':state.phase==='retained'?'Interrupted recording':state.phase==='complete'?'Transcription recovered':'Finish transcription';
    const labels={partial:'Available saved speech recovered. The final recording time remains unknown and additional original audio is retained.',retained:'The final recording time is unknown. Audio and notes are retained. Available saved speech may be recovered locally using its original model; other audio remains pending.',incomplete:'Some saved audio is still unconfirmed. Retry locally to finish it.',preparing:'Preparing the original local model…',
      transcribing:'Recovering saved speech locally…',stopping:'Waiting for local work to stop. Audio is retained.',
      complete:'Recovered speech is saved. Reload the transcript when you are ready.',failed:'Local recovery did not finish. Audio is retained for retry.',cancelled:'Local recovery cancelled. Audio is retained for retry.'};
    message.textContent=state.message||labels[state.phase]||'Recovery status unavailable. Refresh to try again.';
    explanation.hidden=state.phase==='retained'||state.phase==='partial';
    while(sources.firstChild) sources.removeChild(sources.firstChild);
    for(const source of Array.isArray(state.sources)?state.sources:[]) {
      const name=source.source==='system'?'Other participants':source.source==='mic'?'Your microphone':'Capture source';
      const seconds=Number.isFinite(source.pending_audio_ms)?Math.max(0,source.pending_audio_ms/1000).toFixed(1):'Unknown';
      sources.appendChild(make('p',name+' · '+seconds+'s unconfirmed · '+String(source.model||'Original model')));
    }
    retry.disabled=running||state.can_retry!==true;
    cancel.hidden=!running;cancel.disabled=state.phase==='stopping';
    refresh.disabled=false;reloadButton.hidden=state.phase!=='complete'&&state.phase!=='partial';
    if(state.phase==='complete'&&!hasRun) root.hidden=true;
    if(running||(state.can_retry===false&&state.phase==='incomplete')) poll();
  }
  async function load(action) {
    if(root.isConnected===false) return;
    const seq=++request;
    if(action) retry.disabled=true;
    refresh.disabled=true;
    try {
      const state=await api(base+(action?'/'+action:''),action?{method:'POST',headers:{'content-type':'application/json'},body:'{}'}:undefined);
      if(!current(seq)) return;
      render(state);
      if(action==='retry'&&!cancel.hidden) cancel.focus();
      else if(action==='cancel') refresh.focus();
    } catch(error) {
      if(!current(seq)) return;
      root.hidden=error.status===404;
      message.textContent=error.code==='saved_audio_unavailable'?'Saved recording could not be validated for retry. Check its original audio, then refresh status.':error.status===409?'Another local operation is still pending. Refresh status before retrying.':'Could not check saved speech. Refresh status to try again.';
      refresh.disabled=false;retry.disabled=true;
    }
  }
  retry.addEventListener('click',()=>load('retry'));
  cancel.addEventListener('click',()=>load('cancel'));
  refresh.addEventListener('click',()=>load());
  reloadButton.addEventListener('click',()=>{if(root.isConnected!==false) reload();});
  load();
  return root;
}};

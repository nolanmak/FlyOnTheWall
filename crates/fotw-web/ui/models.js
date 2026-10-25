// Provisioning never changes the saved speech policy or starts on selection.
globalThis.FOTWModels = { mount(parent, api) {
  const make=(tag,value,id)=>{const n=document.createElement(tag);if(value)n.textContent=value;if(id)n.id=id;return n;};
  const root=make('section','','model-setup');root.className='model-setup';root.hidden=true;parent.appendChild(root);
  root.appendChild(make('h3','Install a speech model'));
  root.appendChild(make('p','Download explicitly while online, or import a file already on this Mac. Stop recording before setup. Installing a file does not change your speech mode or start transcription.','model-explanation'));
  const label=root.appendChild(make('label','Model tier'));label.className='gh-field';
  const choice=label.appendChild(make('select','','model-choice'));
  const metadata=root.appendChild(make('p','','model-metadata'));metadata.className='model-metadata';
  const license=root.appendChild(make('a','Read the pinned model license'));license.target='_blank';license.rel='noopener noreferrer';
  const actions=root.appendChild(make('div'));actions.className='model-actions';
  const download=actions.appendChild(make('button','Download selected model','model-download'));
  const pathLabel=root.appendChild(make('label','Full path to a local model file'));pathLabel.className='gh-field';
  const path=pathLabel.appendChild(make('input','','model-import-path'));path.type='text';path.maxLength=4096;path.autocomplete='off';path.spellcheck=false;
  const importButton=root.appendChild(make('button','Verify and import local file','model-import'));
  const progress=root.appendChild(make('progress','','model-progress'));progress.setAttribute('aria-label','Model setup bytes');progress.hidden=true;
  const message=root.appendChild(make('p','Loading model catalog…','model-status'));message.setAttribute('role','status');
  const controls=root.appendChild(make('div'));controls.className='model-actions';
  const cancel=controls.appendChild(make('button','Cancel setup','model-cancel'));
  const refresh=controls.appendChild(make('button','Refresh setup status','model-refresh'));
  for(const button of [download,importButton,cancel,refresh])button.type='button';
  let current=null,models=[],catalog='',busy=false,confirmed=false,sequence=0,timer=false;
  function paint() {
    const running=current?.worker_running===true,known=models.some(m=>m.name===choice.value);
    download.disabled=importButton.disabled=busy||!confirmed||running||!known;
    choice.disabled=busy||running;path.disabled=busy||running;
    cancel.hidden=!running;cancel.disabled=busy||!confirmed||['stopping','installing','verified'].includes(current?.phase);
    refresh.disabled=busy;
  }
  function describe() {
    const m=models.find(m=>m.name===choice.value);
    metadata.textContent=m?`${m.name} · ${m.description} · ${m.bytes} bytes · ${m.license}\nRevision: ${m.revision}\nSHA-256: ${m.sha256}\n${m.present?'File present; contents and native readiness are checked before use.':'File not installed in this library.'}`:'';
    const safe=m?.license_url?.startsWith('https://');license.hidden=!safe;if(safe)license.href=m.license_url;
    download.textContent=m?'Download '+m.bytes+' bytes':'Download selected model';paint();
  }
  function poll() {
    if(timer||root.isConnected===false)return;
    timer=true;setTimeout(async()=>{timer=false;if(root.isConnected!==false&&!busy)await load();},1000);
  }
  function render(state) {
    root.hidden=false;current=state.job;models=Array.isArray(state.models)?state.models:[];
    const next=JSON.stringify(models);
    if(catalog!==next) {
      const selected=choice.value;while(choice.firstChild)choice.removeChild(choice.firstChild);
      for(const model of models){const option=make('option',model.name+' · '+model.description);option.value=model.name;choice.appendChild(option);}
      choice.value=models.some(m=>m.name===selected)?selected:models.find(m=>m.name==='small.en')?.name||models[0]?.name||'';catalog=next;
    }
    describe();
    const labels={idle:'Choose a model and explicitly download or import it.',checking:'Checking existing model files…',transferring:'Transferring model bytes…',verifying:'Verifying model size and SHA-256…',installing:'Installing the verified model…',verified:'Model file verified. Select its name above and Save transcription. Native readiness is checked before Start.',stopping:'Waiting for setup IO to finish before retrying.',cancelled:'Setup cancelled. You can retry.',failed:'Setup failed. Refresh and retry; check available disk space and the selected file.'};
    message.textContent=current?.message||labels[current?.phase]||'Unknown setup status. Refresh before trying again.';
    if(current?.worker_running&&current.phase==='verified')message.textContent='Model file verified; finishing setup cleanup…';
    if(current?.model)message.textContent=current.model+" · "+message.textContent;
    progress.hidden=!current?.worker_running;
    if(current?.total>0){progress.max=current.total;progress.value=Math.min(current.total,Math.max(0,current.bytes||0));if(current.worker_running)message.textContent+=' '+progress.value+' / '+current.total+' bytes.';}
    if(current?.worker_running)poll();paint();
  }
  async function load(action,body) {
    if(root.isConnected===false||busy)return;
    const seq=++sequence;if(action){busy=true;paint();}
    try {
      const state=await api('/api/models'+(action?'/'+action:''),action?{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)}:undefined);
      if(seq!==sequence||root.isConnected===false)return;
      confirmed=true;render(state);
    } catch(error) {
      if(seq!==sequence||root.isConnected===false)return;
      confirmed=false;root.hidden=error.status===404;
      message.textContent=(typeof error.code==='string'?error.code.slice(0,500)+' ':'')+'Setup status could not be confirmed. Refresh setup status before another action.';
    } finally {if(seq===sequence){busy=false;paint();if(current?.worker_running)poll();}}
  }
  choice.addEventListener('change',describe);
  download.addEventListener('click',()=>{if(!download.disabled)return load('install',{source:'download',model:choice.value});});
  importButton.addEventListener('click',()=>{if(!importButton.disabled)return load('install',{source:'import',model:choice.value,path:path.value});});
  cancel.addEventListener('click',()=>{if(!cancel.disabled&&current?.worker_running)return load('cancel',{job_id:current.id});});
  refresh.addEventListener('click',()=>load());
  paint();load();return root;
}};

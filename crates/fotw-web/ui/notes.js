/* Note drafts stay in memory; durable text belongs only in the encrypted store. */
(function (root) {
  class Draft {
    constructor(id, api, notify = () => {}) {
      this.id=id; this.api=api; this.notify=notify;
      this.body=''; this.anchors=[]; this.revision=0; this.dirty=false;
      this.status='loading'; this.version=0; this.pending=null; this.loaded=false;
    }
    path() { return '/api/meetings/'+encodeURIComponent(this.id)+'/notes'; }
    changed() { this.notify(this); }
    async load() {
      if (this.dirty) return;
      this.status='loading';this.changed();
      try {
        const d=await this.api(this.path());
        this.body=d.body_md;this.revision=d.revision;this.anchors=d.anchors;
        this.loaded=true;this.status='saved';
      } catch (_) { this.status='error'; }
      this.changed();
    }
    edit(body) {
      if (!this.loaded) return;
      this.body=body;this.version++;this.dirty=true;
      if (this.status!=='conflict') this.status='unsaved';
      this.changed();
    }
    async flush() {
      if (this.pending) return this.pending;
      if (!this.dirty || this.status==='conflict' || !this.loaded) return;
      this.pending=this.saveLoop();
      try { await this.pending; } finally { this.pending=null;this.changed(); }
    }
    async saveLoop() {
      while (this.dirty) {
        const version=this.version;
        const payload={revision:this.revision,body_md:this.body,anchors:this.anchors};
        this.status='saving';this.changed();
        try {
          const saved=await this.api(this.path(), {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(payload)});
          this.revision=saved.revision;
          this.dirty=version!==this.version;
          this.status=this.dirty?'unsaved':'saved';
        } catch (e) {
          this.status=e.status===409?'conflict':'error';
          this.changed();return;
        }
        this.changed();
      }
    }
    async overwrite() {
      if(this.pending || this.status!=='conflict')return;
      // The user explicitly chose their draft. Refresh only the revision, not text.
      try {
        const current=await this.api(this.path());
        this.revision=current.revision;this.status='unsaved';
        await this.flush();
      } catch (_) { this.status='conflict';this.changed(); }
    }
    async discard() {
      if(this.pending)return;
      this.dirty=false;await this.load();
    }
  }

  const drafts=new Map();
  function flushAll() { return Promise.all([...drafts.values()].map(d=>d.flush())); }
  function mount(id, parent, api) {
    const make=(tag,text)=>{const n=document.createElement(tag);if(text)n.textContent=text;return n;};
    const section=make('section');section.className='notes note-editor';
    section.appendChild(make('h3','Your notes'));
    const label=make('label','Notes for this meeting');
    const input=make('textarea');input.rows=7;input.disabled=true;
    label.appendChild(input);section.appendChild(label);
    const status=make('p');status.setAttribute('role','status');section.appendChild(status);
    const actions=make('div');actions.className='actions';
    const retry=make('button','Save / retry');retry.type='button';
    const overwrite=make('button','Replace saved notes with my draft');overwrite.type='button';
    const reload=make('button','Discard my draft and reload');reload.type='button';
    for(const b of [retry,overwrite,reload])actions.appendChild(b);
    section.appendChild(actions);parent.appendChild(section);
    let draft=drafts.get(id);
    if(!draft){draft=new Draft(id,api);drafts.set(id,draft);}
    let delay=null, maximum=null;
    const cancelTimers=()=>{clearTimeout(delay);clearTimeout(maximum);delay=maximum=null;};
    const flush=()=>{cancelTimers();return draft.flush();};
    const labels={loading:'Loading notes…',saved:'Saved',unsaved:'Unsaved changes',saving:'Saving…',error:'Could not save or load notes. Your unsaved draft is kept in this tab.',conflict:'Another tab changed these notes. Your draft is kept; choose which version to use.'};
    draft.notify=()=>{
      if(input.value!==draft.body)input.value=draft.body;
      input.disabled=!draft.loaded;
      status.textContent=labels[draft.status];
      retry.disabled=draft.status==='saving'||draft.status==='loading'||draft.status==='conflict';
      overwrite.hidden=draft.status!=='conflict';
      reload.hidden=!draft.dirty;
      reload.disabled=!!draft.pending;
    };
    input.addEventListener('input',()=>{
      draft.edit(input.value);clearTimeout(delay);delay=setTimeout(flush,500);
      if(maximum===null)maximum=setTimeout(flush,5000);
    });
    input.addEventListener('blur',flush);
    retry.addEventListener('click',()=>draft.loaded?flush():draft.load());
    overwrite.addEventListener('click',()=>draft.overwrite());
    reload.addEventListener('click',()=>{cancelTimers();draft.discard();});
    draft.notify();
    if(!draft.loaded)draft.load();
  }
  root.FOTWNotes={Draft,mount,flushAll};
  if(typeof window!=='undefined' && window.addEventListener) {
    window.addEventListener('beforeunload',e=>{
      if([...drafts.values()].some(d=>d.dirty)) { e.preventDefault();e.returnValue=''; }
    });
  }
  if (typeof module !== 'undefined') module.exports=root.FOTWNotes;
})(globalThis);

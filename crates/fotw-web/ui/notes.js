/* Note drafts stay in memory; durable text belongs only in the encrypted store. */
(function (root) {
  // Spans retain the exact Markdown bytes; parsing never rewrites the note.
  // Paragraphs, list items, headings and fenced code are separate editor blocks.
  function blocks(body) {
    const result=[];
    let current=null, fence=null, offset=0;
    const finish=()=>{if(current){current.text=body.slice(current.start,current.end);result.push(current);current=null;}};
    for(const raw of body.split(/(?<=\n)/)) {
      const line=raw.replace(/\r?\n$/, '');
      const marker=/^ {0,3}(`{3,}|~{3,})/.exec(line);
      if(fence) {
        current.end=offset+line.length;
        if(marker && marker[1][0]===fence[0] && marker[1].length>=fence.length && /^ {0,3}(`+|~+)\s*$/.test(line)) {fence=null;finish();}
      } else if(marker) {
        finish();current={start:offset,end:offset+line.length};fence=marker[1];
      } else if(!line.trim()) {finish();}
      else {
        const separate=/^ {0,3}(?:#{1,6}\s|[-+*]\s|\d+[.)]\s|>)/.test(line);
        if(separate)finish();
        if(!current)current={start:offset,end:offset+line.length};
        current.end=offset+line.length;
        if(/^ {0,3}#{1,6}\s/.test(line))finish();
      }
      offset+=raw.length;
    }
    finish();return result;
  }

  function remapAnchors(before, after, anchors, selection) {
    if(before===after)return anchors;
    const old=blocks(before), next=blocks(after), used=new Set(), mapped=[];
    let start=0;
    while(start<before.length && start<after.length && before[start]===after[start])start++;
    let oldEnd=before.length,newEnd=after.length;
    while(oldEnd>start && newEnd>start && before[oldEnd-1]===after[newEnd-1]){oldEnd--;newEnd--;}
    // Native beforeinput provides the actual replaced range. Prefer it when it
    // explains the resulting text (not all IME/delete operations expose one).
    if(selection && Number.isInteger(selection.start) && Number.isInteger(selection.end)
        && selection.start>=0 && selection.end>=selection.start && selection.end<=before.length) {
      const end=after.length-(before.length-selection.end);
      if(end>=selection.start && before.slice(0,selection.start)===after.slice(0,selection.start)
          && before.slice(selection.end)===after.slice(end)) {
        start=selection.start;oldEnd=selection.end;newEnd=end;
      }
    }
    const delta=newEnd-oldEnd;
    const candidates=[...anchors].sort((a,b)=>a.block_idx-b.block_idx)
      .map(anchor=>({anchor,source:old[anchor.block_idx]}))
      .filter(({anchor,source})=>source && source.text===anchor.block_text);
    const assign=(anchor,index)=>{used.add(index);mapped.push({...anchor,block_idx:index,block_text:next[index].text});};
    const remaining=[];
    // First reserve blocks outside the edited span. Otherwise editing the first
    // of two identical paragraphs could steal the second paragraph's timestamp.
    for(const item of candidates) {
      const {anchor,source}=item;
      const position=source.end<=start ? source.start : source.start>=oldEnd ? source.start+delta : null;
      const index=position===null ? -1 : next.findIndex((b,i)=>!used.has(i) && b.start===position && b.text===source.text);
      if(index>=0)assign(anchor,index);else remaining.push(item);
    }
    const edited=[];
    // A whole-document reorder can move a snapshot outside the inferred edit.
    for(const item of remaining) {
      const index=next.findIndex((b,i)=>!used.has(i) && b.text===item.source.text);
      if(index>=0)assign(item.anchor,index);else edited.push(item);
    }
    for(const {anchor,source} of edited) {
      const inside=start>=source.start && oldEnd<=source.end;
      const prefixSurvives=source.start<start && source.end>=start;
      const suffixSurvives=source.start<=oldEnd && source.end>oldEnd;
      if(!inside && !prefixSurvives && !suffixSurvives)continue;
      // A split retains the leading block's anchor; a merge retains the leading
      // surviving anchor. New saved-meeting blocks never acquire historic times.
      const position=source.start<=start ? source.start : newEnd;
      const index=next.findIndex((b,i)=>!used.has(i) && b.start<=position && b.end>position);
      if(index>=0)assign(anchor,index);
    }
    return mapped.sort((a,b)=>a.block_idx-b.block_idx);
  }

  class RecordingClock {
    constructor(now=()=>Date.now()) { this.now=now; }
    update(status) {
      if(status.session_id!==this.status?.session_id)this.stopping=false;
      this.status=status;this.received=this.now();
    }
    stop() { this.stopping=true; }
    retry() { this.stopping=false; }
    offset(id) {
      const s=this.status;
      if(this.stopping || !s || s.state!=='recording' || s.session_id!==id
          || !Number.isFinite(s.elapsed_ms) || !Number.isFinite(s.started_at_ms))return null;
      const elapsed=Math.max(0,s.elapsed_ms)+Math.max(0,this.now()-this.received);
      if(Number.isFinite(s.auto_stop_at_ms) && elapsed>=s.auto_stop_at_ms-s.started_at_ms)return null;
      return Math.floor(elapsed);
    }
  }

  class Draft {
    constructor(id, api, notify = () => {}, options = {}) {
      this.id=id; this.api=api; this.notify=notify;this.options=options;this.savedMeetingId=null;
      this.body=''; this.anchors=[]; this.revision=0; this.dirty=false;
      this.status='loading'; this.version=0; this.pending=null; this.loaded=false;
      this.composing=false;this.view=null;this.captureElapsed=null;this.reading=0;this.readVersion=0;
    }
    path() { return '/api/'+(this.options.session?'sessions':'meetings')+'/'+encodeURIComponent(this.id)+'/notes'; }
    document(response) {
      if(!this.options.session)return response;
      this.savedMeetingId=response.meeting_id || this.savedMeetingId;
      if(this.savedMeetingId && Number.isFinite(response.elapsed_ms)) {
        // A final status may arrive after typing. Preserve the text, but do not
        // pretend blocks typed beyond capture's end were spoken during it.
        this.captureElapsed=response.elapsed_ms;
        const bounded=this.anchors.filter(a=>a.typed_at_ms<=this.captureElapsed);
        if(bounded.length!==this.anchors.length) {this.anchors=bounded;this.version++;this.dirty=true;}
      }
      if(this.savedMeetingId)linkSession(this.id,this.savedMeetingId);
      return response.note;
    }
    changed() { this.notify(this);trimDraft(this); }
    async load() {
      if (this.dirty || this.composing) return;
      const version=this.version, previousStatus=this.status, readVersion=++this.readVersion;
      this.reading++;
      this.status='loading';this.changed();
      try {
        const response=await this.api(this.path());
        // A remount may have started a newer read. Its body, revision and final
        // capture clock must not be replaced by an older request's snapshot.
        if(readVersion!==this.readVersion)return;
        const d=this.document(response);
        if(this.version!==version || this.dirty || this.composing) {
          if(this.status==='loading')this.status=this.dirty?'unsaved':previousStatus;
          this.changed();return;
        }
        this.body=d.body_md;this.revision=d.revision;this.anchors=d.anchors;
        this.loaded=true;this.status='saved';
      } catch (_) { if(readVersion===this.readVersion)this.status='error'; }
      finally {this.reading--;trimDraft(this);}
      this.changed();
    }
    edit(body, selection, checkpoint) {
      if (!this.loaded || body===this.body) return;
      // Track unanchored old blocks too: loading/editing old text must never
      // manufacture its first-keystroke time. Only genuinely new blocks anchor.
      const before=checkpoint?.body ?? this.body;
      const sourceAnchors=checkpoint?.anchors ?? this.anchors;
      // A final clock learned during composition also bounds its older checkpoint.
      const anchors=Number.isFinite(this.captureElapsed)?sourceAnchors.filter(a=>a.typed_at_ms<=this.captureElapsed):sourceAnchors;
      const tracked=blocks(before).map((block,index)=>anchors.find(a=>a.block_idx===index)
        || {id:null,block_idx:index,block_text:block.text,typed_at_ms:null});
      const mapped=remapAnchors(before,body,tracked,before===this.body?selection:null);
      this.anchors=mapped.filter(a=>a.id!==null);
      const offset=this.options.session && !this.savedMeetingId ? this.options.clock?.() : null;
      if(Number.isFinite(offset) && offset>=0) {
        for(const [index,block] of blocks(body).entries()) {
          if(mapped.some(a=>a.block_idx===index))continue;
          const id=this.options.newId ? this.options.newId() : root.crypto.randomUUID();
          this.anchors.push({id,block_idx:index,block_text:block.text,typed_at_ms:Math.floor(offset)});
        }
        this.anchors.sort((a,b)=>a.block_idx-b.block_idx);
      }
      this.body=body;this.version++;this.dirty=true;
      if (this.status!=='conflict') this.status='unsaved';
      this.changed();
    }
    moveBlock(index, direction) {
      const spans=blocks(this.body), target=index+direction;
      if(!this.loaded || !Number.isInteger(index) || ![-1,1].includes(direction)
          || index<0 || index>=spans.length || target<0 || target>=spans.length)return false;
      const parts=spans.map(b=>b.text);
      [parts[index],parts[target]]=[parts[target],parts[index]];
      // Explicit movement separates blocks with blank lines. Inner Markdown,
      // including fenced code and soft line breaks, remains byte-for-byte intact.
      this.body=this.body.slice(0,spans[0].start)+parts.join('\n\n')+this.body.slice(spans.at(-1).end);
      this.anchors=this.anchors.filter(a=>spans[a.block_idx]?.text===a.block_text).map(a=>({...a,block_idx:a.block_idx===index?target:a.block_idx===target?index:a.block_idx}))
        .sort((a,b)=>a.block_idx-b.block_idx);
      this.version++;this.dirty=true;
      if(this.status!=='conflict')this.status='unsaved';
      this.changed();return true;
    }
    async flush(force=false) {
      if(force)this.view?.prepare();
      if (this.pending) {
        await this.pending;
        // A composition can commit after saveLoop exits but before its owner
        // clears pending. Drain that committed edit, never a failed/conflicted save.
        if(this.dirty && !this.composing && this.status==='unsaved')return this.flush();
        return;
      }
      if (this.composing || !this.dirty || this.status==='conflict' || !this.loaded) return;
      this.pending=this.saveLoop();
      try { await this.pending; } finally { this.pending=null;this.changed(); }
    }
    async saveLoop() {
      while (this.dirty && !this.composing) {
        const version=this.version;
        const payload={revision:this.revision,body_md:this.body,anchors:this.anchors};
        this.status='saving';this.changed();
        try {
          const response=await this.api(this.path(), {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(payload)});
          const saved=this.document(response);
          this.revision=saved.revision;
          this.dirty=version!==this.version;
          this.status=this.dirty?'unsaved':'saved';
        } catch (e) {
          this.status=e.status===409 && e.code!=='recording_clock_unavailable'?'conflict':'error';
          this.changed();return;
        }
        this.changed();
      }
    }
    async retrySave() {
      if(this.pending)await this.pending;
      if(this.status==='conflict' || !this.dirty)return;
      if(this.options.session) {
        const readVersion=++this.readVersion;this.reading++;
        try {
          const response=await this.api(this.path());
          if(readVersion!==this.readVersion)return;
          this.document(response);
        } catch (_) {
          if(readVersion===this.readVersion){this.status='error';this.changed();}
          return;
        } finally {this.reading--;trimDraft(this);}
      }
      await this.flush();
    }
    async overwrite() {
      if(this.pending || this.status!=='conflict')return;
      // The user explicitly chose their draft. Refresh only the revision, not text.
      const readVersion=++this.readVersion;this.reading++;
      try {
        const response=await this.api(this.path());
        if(readVersion!==this.readVersion)return;
        const current=this.document(response);
        this.revision=current.revision;this.status='unsaved';
        await this.flush();
      } catch (_) { if(readVersion===this.readVersion){this.status='conflict';this.changed();} }
      finally {this.reading--;trimDraft(this);}
    }
    async discard() {
      if(this.pending)return;
      this.dirty=false;await this.load();
    }
  }

  const drafts=new Map();
  function trimDraft(draft) {
    if(draft.view || draft.dirty || draft.pending || draft.composing || draft.reading)return;
    // A live draft can have both session and canonical-meeting keys. Release
    // only aliases to this clean object; independently edited drafts survive.
    for(const [key,value] of drafts)if(value===draft)drafts.delete(key);
  }
  function linkSession(sessionId, meetingId) {
    const draft=drafts.get('session:'+sessionId);
    if(draft) {
      draft.savedMeetingId=meetingId;
      // Never discard an independently edited saved-meeting draft.
      if(!drafts.has('meeting:'+meetingId))drafts.set('meeting:'+meetingId,draft);
    }
  }
  async function finishSession(sessionId, meetingId) {
    linkSession(sessionId,meetingId);
    const draft=drafts.get('session:'+sessionId);
    if(draft)await draft.retrySave();
  }
  function flushAll() { return Promise.all([...new Set(drafts.values())].map(d=>d.flush(true))); }
  function release(parent) {
    return Promise.all([...new Set(drafts.values())].map(d=>{
      const view=d.view;
      if(view && (view.parent===parent || parent.contains?.(view.section)))return view.release();
    }));
  }
  function mount(id, parent, api, options = {}) {
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
    const up=make('button','Move block up'), down=make('button','Move block down');
    for(const b of [up,down])b.type='button';
    for(const b of [retry,overwrite,reload,up,down])actions.appendChild(b);
    section.appendChild(actions);parent.appendChild(section);
    const key=(options.session?'session:':'meeting:')+id;
    drafts.get(key)?.view?.release();
    let draft=drafts.get(key);
    if(!draft){draft=new Draft(id,api,()=>{},options);drafts.set(key,draft);}
    let composing=false, checkpoint=null;const view={parent,section};draft.view=view;
    const active=()=>draft.view===view && section.isConnected!==false;
    let delay=null, maximum=null;
    const cancelTimers=()=>{clearTimeout(delay);clearTimeout(maximum);delay=maximum=null;};
    const flush=()=>{cancelTimers();return draft.flush();};
    const schedule=()=>{clearTimeout(delay);delay=setTimeout(flush,500);if(maximum===null)maximum=setTimeout(flush,5000);};
    const selectedBlock=()=>blocks(draft.body).findIndex(b=>input.selectionStart>=b.start && input.selectionStart<=b.end);
    const updateMovement=()=>{
      const index=selectedBlock(), count=blocks(draft.body).length;
      up.disabled=composing || !draft.loaded || index<=0;
      down.disabled=composing || !draft.loaded || index<0 || index>=count-1;
    };
    const labels={loading:'Loading notes…',saved:'Saved',unsaved:'Unsaved changes',saving:'Saving…',error:'Could not save or load notes. Your unsaved draft is kept in this tab.',conflict:'Another tab changed these notes. Your draft is kept; choose which version to use.'};
    draft.notify=()=>{
      if(!active())return;
      if(!composing && input.value!==draft.body)input.value=draft.body;
      input.disabled=!draft.loaded;
      status.textContent=composing?'Composing… Your draft is kept in this tab until committed. '+(draft.status==='conflict'?labels.conflict:''):labels[draft.status];
      retry.disabled=composing||draft.status==='saving'||draft.status==='loading'||draft.status==='conflict';
      overwrite.hidden=draft.status!=='conflict';overwrite.disabled=composing;
      reload.hidden=!draft.dirty;
      reload.disabled=composing||!!draft.pending;
      updateMovement();
    };
    let selection=null;
    const beginComposition=()=>{
      composing=draft.composing=true;checkpoint={body:draft.body,anchors:draft.anchors};
      cancelTimers();draft.changed();
    };
    input.addEventListener('beforeinput',event=>{
      if(!active())return;
      if(event?.isComposing && !composing)beginComposition();
      selection={start:input.selectionStart,end:input.selectionEnd};
    });
    const editVisible=()=>{
      draft.edit(input.value,selection,composing?checkpoint:undefined);selection=null;
      // IMEs can temporarily remove the marked range before inserting a commit.
      // Retain the last complete block mapping through that deletion, including
      // old unanchored blocks. Grow the checkpoint when new blocks appear.
      if(composing && (!checkpoint || (blocks(draft.body).length>=blocks(checkpoint.body).length
          && checkpoint.anchors.every(a=>draft.anchors.some(b=>b.id===a.id))))) {
        checkpoint={body:draft.body,anchors:draft.anchors};
      }
    };
    view.prepare=()=>{
      if(draft.view!==view)return;
      draft.composing=false;
      // Native textareas normalize line endings. Only active composition can
      // contain text not yet reported by input; never resave an untouched field.
      if(composing && section.isConnected!==false)editVisible();
      selection=null;cancelTimers();
    };
    view.release=()=>{
      if(draft.view!==view)return;
      view.prepare();composing=false;checkpoint=null;draft.view=null;draft.notify=()=>{};
      const saving=draft.flush();trimDraft(draft);return saving;
    };
    input.addEventListener('compositionstart',()=>{
      if(!active())return;
      beginComposition();
    });
    input.addEventListener('input',event=>{
      if(!active())return;
      if(event?.isComposing && !composing)beginComposition();
      editVisible();
      if(composing){cancelTimers();return;}
      schedule();
    });
    input.addEventListener('compositionend',()=>{
      if(!active())return;
      editVisible();composing=draft.composing=false;checkpoint=null;
      draft.changed();flush();
    });
    for(const event of ['select','click','keyup'])input.addEventListener(event,updateMovement);
    const move=direction=>{
      if(!active() || composing)return;
      const index=selectedBlock();
      if(draft.moveBlock(index,direction)) {
        const block=blocks(draft.body)[index+direction];
        input.focus();input.setSelectionRange(block.start,block.end);
        updateMovement();flush();
      }
    };
    up.addEventListener('click',()=>move(-1));
    down.addEventListener('click',()=>move(1));
    input.addEventListener('blur',()=>{if(active()){cancelTimers();return draft.flush(true);}});
    retry.addEventListener('click',()=>{if(!active()||composing)return;cancelTimers();return draft.loaded?draft.retrySave():draft.load();});
    overwrite.addEventListener('click',()=>{if(active()&&!composing)return draft.overwrite();});
    reload.addEventListener('click',()=>{if(!active()||composing)return;cancelTimers();draft.discard();});
    draft.notify();
    if(!draft.loaded)draft.load();
    return draft;
  }
  function recoveryNode(tag, text) {
    const node=document.createElement(tag);if(text)node.textContent=text;return node;
  }
  function mountRecovery(parent,api,onOpen) {
    const section=recoveryNode('section');section.className='note-recovery';
    section.appendChild(recoveryNode('h2','Unfinished notes'));
    section.appendChild(recoveryNode('p','Notes saved locally from recordings that have not finished saving.'));
    const status=recoveryNode('p');status.setAttribute('role','status');section.appendChild(status);
    const rows=recoveryNode('div');rows.className='recovery-list';section.appendChild(rows);
    const more=recoveryNode('button','Load more');more.type='button';
    const refresh=recoveryNode('button','Refresh');refresh.type='button';
    section.appendChild(more);section.appendChild(refresh);parent.appendChild(section);
    let offset=0,busy=false;
    async function load(reset=false) {
      if(busy)return;busy=true;more.disabled=refresh.disabled=true;status.textContent='Loading notes…';
      if(reset){offset=0;rows.textContent='';}
      try {
        const data=await api('/api/note-drafts?limit=50&offset='+offset);
        for(const draft of data.drafts) {
          const row=recoveryNode('button','Recording notes — '+new Date(draft.started_at_ms).toLocaleString());
          row.type='button';row.dataset.sessionId=draft.session_id;
          row.addEventListener('click',()=>onOpen(draft.session_id));rows.appendChild(row);
        }
        offset+=data.drafts.length;more.hidden=data.drafts.length<50;
        status.textContent=offset?'Choose a recording to recover its notes.':'No unfinished notes.';
      } catch (_) {status.textContent='Could not load unfinished notes. Your saved notes are unchanged.';more.hidden=false;}
      finally {busy=false;more.disabled=refresh.disabled=false;}
    }
    more.addEventListener('click',()=>load());refresh.addEventListener('click',()=>load(true));
    load();return {load};
  }
  function downloadRecovered(filename, text, type) {
    const url=URL.createObjectURL(new Blob([text],{type}));
    const link=recoveryNode('a');link.href=url;link.download=filename;link.click();
    setTimeout(()=>URL.revokeObjectURL(url),1000);
  }
  function mountRecovered(id,parent,api,options={}) {
    const section=recoveryNode('section');section.className='notes note-editor';
    section.appendChild(recoveryNode('h2','Notes from an unfinished recording'));
    section.appendChild(recoveryNode('p','You can copy or download these notes. The original stays saved locally.'));
    const label=recoveryNode('label','Recovered notes'),input=recoveryNode('textarea');
    input.readOnly=true;input.rows=12;label.appendChild(input);section.appendChild(label);
    const status=recoveryNode('p');status.setAttribute('role','status');section.appendChild(status);
    const actions=recoveryNode('div');actions.className='actions';
    const copy=recoveryNode('button','Copy notes'),md=recoveryNode('button','Download Markdown'),json=recoveryNode('button','Download JSON'),retry=recoveryNode('button','Reload');
    for(const button of [copy,md,json,retry]){button.type='button';actions.appendChild(button);}
    section.appendChild(actions);parent.appendChild(section);let snapshot=null,busy=false;
    const buttons=()=>{for(const button of [copy,md,json])button.disabled=!snapshot||busy;retry.disabled=busy;};
    async function load() {
      if(busy)return;busy=true;buttons();status.textContent='Loading notes…';
      try {
        const doc=await api('/api/sessions/'+encodeURIComponent(id)+'/notes');
        if(doc.meeting_id && options.onSaved){options.onSaved(doc.meeting_id);return;}
        snapshot=doc;input.value=doc.note.body_md;status.textContent='Recovered from local storage.';
      }catch(_){status.textContent='Could not load this draft. Retry to recover its saved text.';}
      finally{busy=false;buttons();}
    }
    copy.addEventListener('click',async()=>{
      if(!snapshot||busy)return;busy=true;buttons();
      try {await (options.copy||((text)=>navigator.clipboard.writeText(text)))(snapshot.note.body_md);status.textContent='Notes copied.';}
      catch(_){status.textContent='Could not copy. Select the text and copy it manually, or download it.';}
      finally{busy=false;buttons();}
    });
    const download=kind=>{
      if(!snapshot||busy)return;
      const base='recording-notes-'+id.replace(/[^a-zA-Z0-9_-]/g,'-');
      try {
        (options.download||downloadRecovered)(base+(kind==='json'?'.json':'.md'),kind==='json'?JSON.stringify(snapshot,null,2):snapshot.note.body_md,kind==='json'?'application/json':'text/markdown;charset=utf-8');
        status.textContent='Download requested. The original notes are still saved locally.';
      }catch(_){status.textContent='Could not start the download. Your notes are unchanged.';}
    };
    md.addEventListener('click',()=>download('md'));json.addEventListener('click',()=>download('json'));
    retry.addEventListener('click',load);load();return {load};
  }
  root.FOTWNotes={Draft,RecordingClock,mount,flushAll,release,linkSession,finishSession,mountRecovery,mountRecovered,blocks,remapAnchors};
  if(typeof window!=='undefined' && window.addEventListener) {
    window.addEventListener('beforeunload',e=>{
      if([...drafts.values()].some(d=>d.dirty||d.pending||d.composing)) { e.preventDefault();e.returnValue=''; }
    });
  }
  if (typeof module !== 'undefined') module.exports=root.FOTWNotes;
})(globalThis);

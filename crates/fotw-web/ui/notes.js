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
    }
    path() { return '/api/'+(this.options.session?'sessions':'meetings')+'/'+encodeURIComponent(this.id)+'/notes'; }
    document(response) {
      if(!this.options.session)return response;
      this.savedMeetingId=response.meeting_id || this.savedMeetingId;
      if(this.savedMeetingId && Number.isFinite(response.elapsed_ms)) {
        // A final status may arrive after typing. Preserve the text, but do not
        // pretend blocks typed beyond capture's end were spoken during it.
        const bounded=this.anchors.filter(a=>a.typed_at_ms<=response.elapsed_ms);
        if(bounded.length!==this.anchors.length) {this.anchors=bounded;this.version++;this.dirty=true;}
      }
      if(this.savedMeetingId)linkSession(this.id,this.savedMeetingId);
      return response.note;
    }
    changed() { this.notify(this); }
    async load() {
      if (this.dirty) return;
      this.status='loading';this.changed();
      try {
        const d=this.document(await this.api(this.path()));
        this.body=d.body_md;this.revision=d.revision;this.anchors=d.anchors;
        this.loaded=true;this.status='saved';
      } catch (_) { this.status='error'; }
      this.changed();
    }
    edit(body, selection) {
      if (!this.loaded || body===this.body) return;
      // Track unanchored old blocks too: loading/editing old text must never
      // manufacture its first-keystroke time. Only genuinely new blocks anchor.
      const tracked=blocks(this.body).map((block,index)=>this.anchors.find(a=>a.block_idx===index)
        || {id:null,block_idx:index,block_text:block.text,typed_at_ms:null});
      const mapped=remapAnchors(this.body,body,tracked,selection);
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
        try { this.document(await this.api(this.path())); }
        catch (_) { this.status='error';this.changed();return; }
      }
      await this.flush();
    }
    async overwrite() {
      if(this.pending || this.status!=='conflict')return;
      // The user explicitly chose their draft. Refresh only the revision, not text.
      try {
        const current=this.document(await this.api(this.path()));
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
  function flushAll() { return Promise.all([...new Set(drafts.values())].map(d=>d.flush())); }
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
    let draft=drafts.get(key);
    if(!draft){draft=new Draft(id,api,()=>{},options);drafts.set(key,draft);}
    let delay=null, maximum=null;
    const cancelTimers=()=>{clearTimeout(delay);clearTimeout(maximum);delay=maximum=null;};
    const flush=()=>{cancelTimers();return draft.flush();};
    const selectedBlock=()=>blocks(draft.body).findIndex(b=>input.selectionStart>=b.start && input.selectionStart<=b.end);
    const updateMovement=()=>{
      const index=selectedBlock(), count=blocks(draft.body).length;
      up.disabled=!draft.loaded || index<=0;
      down.disabled=!draft.loaded || index<0 || index>=count-1;
    };
    const labels={loading:'Loading notes…',saved:'Saved',unsaved:'Unsaved changes',saving:'Saving…',error:'Could not save or load notes. Your unsaved draft is kept in this tab.',conflict:'Another tab changed these notes. Your draft is kept; choose which version to use.'};
    draft.notify=()=>{
      if(input.value!==draft.body)input.value=draft.body;
      input.disabled=!draft.loaded;
      status.textContent=labels[draft.status];
      retry.disabled=draft.status==='saving'||draft.status==='loading'||draft.status==='conflict';
      overwrite.hidden=draft.status!=='conflict';
      reload.hidden=!draft.dirty;
      reload.disabled=!!draft.pending;
      updateMovement();
    };
    let selection=null;
    input.addEventListener('beforeinput',()=>{
      selection={start:input.selectionStart,end:input.selectionEnd};
    });
    input.addEventListener('input',()=>{
      draft.edit(input.value,selection);selection=null;clearTimeout(delay);delay=setTimeout(flush,500);
      if(maximum===null)maximum=setTimeout(flush,5000);
    });
    for(const event of ['select','click','keyup'])input.addEventListener(event,updateMovement);
    const move=direction=>{
      const index=selectedBlock();
      if(draft.moveBlock(index,direction)) {
        const block=blocks(draft.body)[index+direction];
        input.focus();input.setSelectionRange(block.start,block.end);
        updateMovement();flush();
      }
    };
    up.addEventListener('click',()=>move(-1));
    down.addEventListener('click',()=>move(1));
    input.addEventListener('blur',flush);
    retry.addEventListener('click',()=>{cancelTimers();return draft.loaded?draft.retrySave():draft.load();});
    overwrite.addEventListener('click',()=>draft.overwrite());
    reload.addEventListener('click',()=>{cancelTimers();draft.discard();});
    draft.notify();
    if(!draft.loaded)draft.load();
    return draft;
  }
  root.FOTWNotes={Draft,RecordingClock,mount,flushAll,linkSession,finishSession,blocks,remapAnchors};
  if(typeof window!=='undefined' && window.addEventListener) {
    window.addEventListener('beforeunload',e=>{
      if([...drafts.values()].some(d=>d.dirty)) { e.preventDefault();e.returnValue=''; }
    });
  }
  if (typeof module !== 'undefined') module.exports=root.FOTWNotes;
})(globalThis);

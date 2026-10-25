// Owned WebKit fixture script. Run in the local synthetic ui_preview page only.
// It replaces fetched meeting data in memory; it never captures audio or writes
// fabricated meetings to the user's library. The optional baseline renderer is
// supplied from the base commit by the external comparison runner.
async function runSavedTranscriptFixture() {
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const wait=async condition=>{const until=Date.now()+10000;while(!condition()){if(Date.now()>until)throw Error('saved transcript fixture timed out');await sleep(30);}};
await wait(()=>document.querySelectorAll('#list .meeting').length===3);
const list=await api('/api/meetings'), meetings=Array.isArray(list)?list:list.meetings;
const detail=await api('/api/meetings/'+meetings[0].id);
// Isolate the saved transcript renderer from asynchronous notes/document cards.
window.FOTWNotes=undefined;window.FOTWSpeechRecovery=undefined;
mountSharingDocument=()=>{};mountGithubSync=()=>{};
detail.summary_md=null;detail.note_md=null;detail.meeting.enrich_state='disabled';
detail.segments=Array.from({length:12001},(_,idx)=>({idx,start_ms:17000+idx*3000,end_ms:19000+idx*3000,channel:idx%2?'mic':'system',speaker:idx%2?'me':'S0',text:'Synthetic line '+idx+' — 日本語 😀. '+('Long meeting history with readable paragraphs, source timestamps and literal <script>text</script>. ').repeat(3)}));
currentDetail=detail;selectedMeetingId=detail.meeting.id;
const measure=render=>{
 let created=0;const create=document.createElement.bind(document);document.createElement=(...args)=>{created++;return create(...args);};
 const before=performance.now();try{render(detail);void el.detail.offsetHeight;}finally{document.createElement=create;}
 const elapsed=performance.now()-before;
 return {created_elements:created,attached_rows:document.querySelectorAll('#segments .segment').length,layout_ms:elapsed};
};
const result={segments:detail.segments.length,width:innerWidth,appearance:matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light'};
if(window.fixtureBeforeRender)result.before=measure(window.fixtureBeforeRender);
result.after=measure(renderDetail);
if(result.after.attached_rows!==200||result.after.created_elements>1000)throw Error(JSON.stringify(result));
const next=document.querySelector('[aria-label="Next transcript page"]'),field=document.querySelector('[aria-label="Transcript page"]');
field.focus();field.value='61';field.dispatchEvent(new Event('change'));
result.focus_preserved=document.activeElement===field;
result.last_line_reachable=document.getElementById('segments').textContent.includes('Synthetic line 12000');
result.final_clock=document.querySelector('#segments .at').textContent;
result.copy_complete=storedTranscriptPayload(detail).text.includes('Synthetic line 0 —')&&storedTranscriptPayload(detail).text.includes('Synthetic line 12000 —');
result.final_next_disabled=next.disabled;
if(!result.focus_preserved||!result.last_line_reachable||!result.copy_complete||!result.final_next_disabled||result.final_clock!=='600:00')throw Error(JSON.stringify(result));
field.value='60';field.dispatchEvent(new Event('change'));next.focus();next.click();
result.last_boundary_focus=document.activeElement===field;
field.value='2';field.dispatchEvent(new Event('change'));
const previous=document.querySelector('[aria-label="Previous transcript page"]');previous.focus();previous.click();
result.first_boundary_focus=document.activeElement===field;
if(!result.last_boundary_focus||!result.first_boundary_focus)throw Error(JSON.stringify(result));
field.value='1';field.dispatchEvent(new Event('change'));
// The actual focused input receives a native Tab event within this owned window.
field.focus();window.webkit.messageHandlers.fixtureTab.postMessage({option:true});await sleep(100);
result.native_tab_reached_next=document.activeElement===next;
result.next_focus_visible=next.matches(':focus-visible');
result.focus_outline=getComputedStyle(next).outlineStyle;
const range=document.querySelector('.transcript-pages [role="status"]');
result.range=range.textContent;
result.controls_fit=[...document.querySelectorAll('.transcript-pages button,.transcript-pages input')].every(node=>{const r=node.getBoundingClientRect();return r.left>=0&&r.right<=innerWidth;});
result.no_horizontal_overflow=document.documentElement.scrollWidth<=innerWidth;
// This window is deliberately nonactivating: WebKit exposes activeElement but
// does not match :focus/:focus-visible until the window becomes key. Report that
// limitation; do not claim this fixture validates an active-window focus ring.
if(!result.native_tab_reached_next||!result.controls_fit||!result.no_horizontal_overflow)throw Error(JSON.stringify(result));
document.querySelector('.transcript-pages').scrollIntoView({block:'start'});
await sleep(50);
return result;
}

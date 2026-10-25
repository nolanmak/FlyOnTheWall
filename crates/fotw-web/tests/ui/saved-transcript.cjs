const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function harness() {
  class Element {
    constructor(tag) { this.tagName=tag;this.children=[];this.listeners={};this.dataset={};this.classList={add(){},toggle(){}}; }
    get firstChild(){return this.children[0];}
    get textContent(){return (this.ownText||'')+this.children.map(c=>c.textContent).join('');}
    set textContent(t){this.ownText=String(t);this.children=[];}
    appendChild(n){n.parent=this;this.children.push(n);return n;}
    removeChild(n){this.children.splice(this.children.indexOf(n),1);n.parent=null;}
    addEventListener(k,f){this.listeners[k]=f;}
    setAttribute(k,v){this[k]=v;}
    scrollIntoView(){this.scrolled=true;}
    focus(){document.activeElement=this;}
    click(){if(!this.disabled)this.listeners.click?.();}
  }
  const root=new Element('body'),detail=root.appendChild(new Element('section'));detail.id='detail';
  for(const id of ['status','list'])root.appendChild(new Element('div')).id=id;
  const all=(n=root)=>[n,...n.children.flatMap(all)];
  const document={createElement:t=>new Element(t),getElementById:id=>all().find(n=>n.id===id)||null};
  const ctx=vm.createContext({document,Date,Intl,Set,Map,URLSearchParams,window:{location:{search:''}},localStorage:{getItem:()=>null},setInterval:()=>1,clearInterval(){},setTimeout(){},XMLSerializer:class{serializeToString(n){return n.textContent;}}});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8'),ctx);
  for(const name of ['mountGithubSync','mountSharingDocument','paintLiveNavigation'])ctx[name]=()=>{};
  ctx.actionsRow=()=>null;ctx.noSummaryReason=()=>null;
  let rowsCreated=0;const row=ctx.segmentRow;ctx.segmentRow=(...args)=>{rowsCreated++;return row(...args);};
  return {ctx,detail,all,document,get rowsCreated(){return rowsCreated;},button:label=>all().find(n=>n['aria-label']===label)};
}
const segments=n=>Array.from({length:n},(_,idx)=>({idx,start_ms:17000+idx*1000,end_ms:18000+idx*1000,channel:idx%2?'mic':'system',speaker:idx%2?'me':'S0',text:'line '+idx+' 日本語 😀'}));
function render(h,segs){h.ctx.renderDetail({meeting:{id:'saved-A',title:'Synthetic long meeting',started_at_ms:1000},segments:segs});}
test('opening a long saved transcript bounds rendering work and every line remains reachable in order',()=>{
 const h=harness(),segs=segments(4001);render(h,segs);
 assert.ok(h.rowsCreated<=200,'opening must not create thousands of transcript rows');
 const seen=[];
 for(let page=0;page<21;page++){
   const body=h.document.getElementById('segments');
   for(const row of body.children)seen.push(row.children.find(n=>n.className==='words').textContent);
   const before=h.rowsCreated;h.button('Next transcript page').click();
   assert.ok(h.rowsCreated-before<=200,'a page change must have bounded rendering work');
 }
 assert.deepEqual(seen,segs.map(s=>s.text));
 assert.equal(h.button('Next transcript page').disabled,true);
 const copied=h.ctx.storedTranscriptPayload({meeting:{title:'All'},segments:segs}).text;
 assert.deepEqual(copied.match(/line \d+ 日本語 😀/g),segs.map(s=>s.text));
});
test('boundary navigation moves focus before disabling the activated control',()=>{
 const h=harness();render(h,segments(401));
 const next=h.button('Next transcript page'),previous=h.button('Previous transcript page'),field=h.button('Transcript page');
 next.focus();next.click();assert.equal(h.document.activeElement,next);
 next.click();assert.equal(next.disabled,true);assert.equal(h.document.activeElement,field);
 previous.focus();previous.click();assert.equal(h.document.activeElement,previous);
 previous.click();assert.equal(previous.disabled,true);assert.equal(h.document.activeElement,field);
});
test('page selection uses the full speech clock, validates input and retains controls for keyboard focus',()=>{
 const h=harness(),segs=segments(601),before=JSON.stringify(segs);render(h,segs);
 const field=h.all().find(n=>n['aria-label']==='Transcript page');
 assert.ok(field,'page number is keyboard accessible');
 field.value='3';field.listeners.change();
 assert.match(h.document.getElementById('segments').textContent,/6:40S0line 400/);
 assert.equal(h.all().find(n=>n['aria-label']==='Transcript page'),field);
 const count=h.rowsCreated;field.value='NaN';field.listeners.change();assert.equal(field.value,'3');assert.equal(h.rowsCreated,count);
 field.value='999';field.listeners.change();assert.equal(field.value,'4');assert.match(h.document.getElementById('segments').textContent,/line 600/);
 h.button('Previous transcript page').click();assert.equal(field.value,'3');
 assert.equal(JSON.stringify(segs),before);
});
test('short and empty transcripts do not introduce paging and keep sorted speech-relative times',()=>{
 const h=harness();render(h,[{...segments(1)[0],start_ms:36000,text:'later'},{...segments(1)[0],text:'first'}]);
 assert.equal(h.button('Next transcript page'),undefined);
 assert.match(h.document.getElementById('segments').textContent,/0:00S0first0:19later/);
 render(h,[]);assert.equal(h.document.getElementById('segments').children.length,0);
});

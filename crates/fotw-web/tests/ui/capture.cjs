const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function harness() {
  const nodes = new Map();
  const node = id => {
    if (!nodes.has(id)) nodes.set(id,{checked:false,disabled:false,hidden:false,textContent:'',value:'',addEventListener(){}});
    return nodes.get(id);
  };
  const ctx = vm.createContext({document:{getElementById:node},window:{location:{search:''}},URLSearchParams,localStorage:{getItem:()=>null}});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8'),ctx);
  return {ctx,node};
}
const response = (spotify=false,brave=false) => ({settings:{ignore_spotify:spotify,ignore_brave:brave},supported:true,error:null});
test('independent opt-ins render with Brave meeting and microphone disclosures',()=>{
  const {ctx,node}=harness();ctx.renderCaptureSettings(response(true,false));
  assert.equal(node('capture-spotify').checked,true);assert.equal(node('capture-brave').checked,false);
  assert.match(node('capture-disclosure').textContent,/meetings in Brave/);
  assert.match(node('capture-disclosure').textContent,/microphone/);
  assert.match(node('capture-status').textContent,/next recording/);
});
test('save includes both explicit choices and confirms only next-recording policy',async()=>{
  const {ctx,node}=harness();ctx.renderCaptureSettings(response());
  node('capture-brave').checked=true;
  ctx.api=async(url,options)=>{assert.equal(url,'/api/settings/capture');assert.deepEqual(JSON.parse(options.body),{ignore_spotify:false,ignore_brave:true});return response(false,true);};
  await ctx.onCaptureSave();assert.match(node('capture-status').textContent,/next recording/);
});
test('refused save preserves draft and says the preference was not saved',async()=>{
  const {ctx,node}=harness();ctx.renderCaptureSettings(response());node('capture-spotify').checked=true;
  ctx.api=async()=>({...response(),error:'Cannot save'});await ctx.onCaptureSave();
  assert.equal(node('capture-spotify').checked,true);assert.match(node('capture-status').textContent,/Not saved/);
  assert.equal(node('capture-save').disabled,false);
});
test('unsupported runtime permits explicitly disabling a previously saved policy',()=>{
  const {ctx,node}=harness();ctx.renderCaptureSettings({...response(true,true),supported:false});
  assert.match(node('capture-status').textContent,/macOS 26/);
  assert.equal(node('capture-save').disabled,true);
  node('capture-spotify').checked=false;node('capture-brave').checked=false;ctx.paintCapturePolicy();
  assert.equal(node('capture-save').disabled,false);
});
test('transport failure preserves draft and says saving could not be confirmed',async()=>{
  const {ctx,node}=harness();ctx.renderCaptureSettings(response());node('capture-brave').checked=true;
  ctx.api=async()=>{throw new Error('offline')};await ctx.onCaptureSave();
  assert.equal(node('capture-brave').checked,true);assert.match(node('capture-status').textContent,/could not be confirmed/);
});
test('unreadable saved policy never silently becomes an unchecked replacement',()=>{
  const {ctx,node}=harness();ctx.renderCaptureSettings({...response(),settings:null,error:'Corrupt saved policy'});
  assert.equal(node('capture-save').disabled,true);assert.match(node('capture-status').textContent,/Corrupt/);
});

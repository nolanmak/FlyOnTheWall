const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
function harness() {
  const nodes = new Map();
  const node = id => {
    if (!nodes.has(id)) nodes.set(id, {value:'',textContent:'',hidden:false,disabled:false,checked:false,addEventListener(){}});
    return nodes.get(id);
  };
  const ctx = vm.createContext({document:{getElementById:node},window:{location:{search:''}},URLSearchParams,localStorage:{getItem:()=>null}});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8'),ctx);
  return {ctx,node};
}
const response = (engine='local') => ({settings:{engine,model:'/models/small.en.bin',language:'en'},model_availability:'present_unchecked',model_path:'/models/small.en.bin',error:null});
test('invalid saved settings never select cloud and remain repairable',()=>{
  const {ctx,node}=harness();ctx.renderSpeechSettings({...response(),settings:null,model_availability:'unknown',error:'Saved choice is invalid'});
  assert.equal(node('speech-mode').value,'');assert.equal(node('speech-save').disabled,true);
  assert.match(node('speech-status').textContent,/invalid/);
  node('speech-mode').value='local';ctx.paintSpeechPolicy();assert.equal(node('speech-save').disabled,false);
});
test('policy disclosure distinguishes local speech from cloud speech and summaries',()=>{
  const {ctx,node}=harness();ctx.renderSpeechSettings(response());
  assert.match(node('speech-disclosure').textContent,/stays on this Mac/);
  assert.match(node('speech-disclosure').textContent,/Summar/);
  node('speech-mode').value='fallback';ctx.paintSpeechPolicy();
  assert.match(node('speech-disclosure').textContent,/Deepgram/);
  assert.match(node('speech-disclosure').textContent,/rest of the meeting/);
  assert.match(node('speech-status').textContent,/Start/);
});
test('refused save preserves model draft and submits an explicit fallback policy',async()=>{
  const {ctx,node}=harness();ctx.renderSpeechSettings(response());
  node('speech-mode').value='fallback';node('speech-model').value='/draft/bad.bin';let submitted;
  ctx.api=async(url,options)=>{assert.equal(url,'/api/settings/speech');submitted=JSON.parse(options.body);return {...response(),error:'Model checksum mismatch'};};
  await ctx.onSpeechSave();assert.equal(submitted.engine,'fallback');assert.equal(submitted.model,'/draft/bad.bin');
  assert.equal(node('speech-model').value,'/draft/bad.bin');assert.match(node('speech-status').textContent,/checksum/);
  assert.equal(node('speech-save').disabled,false);
});
test('temporary settings failure is visible and retryable, and absent control hides the panel',async()=>{
  const {ctx,node}=harness();ctx.api=async()=>{throw Object.assign(new Error('offline'),{status:500});};
  await ctx.loadSpeech();assert.equal(node('speech-settings').hidden,false);assert.match(node('speech-status').textContent,/Retry/);
  ctx.api=async()=>response('fallback');await ctx.loadSpeech();assert.equal(node('speech-mode').value,'fallback');
  ctx.api=async()=>{throw Object.assign(new Error('absent'),{status:404});};await ctx.loadSpeech();assert.equal(node('speech-settings').hidden,true);
});
// #245: live captions are a separate, saved choice with an install hint.
test('live captions show the saved choice and are sent on save',async()=>{
  const {ctx,node}=harness();
  ctx.renderSpeechSettings({...response(),settings:{...response().settings,live_captions:false},captions_installed:true});
  assert.equal(node('speech-captions').checked,false);
  ctx.renderSpeechSettings({...response(),captions_installed:true});
  assert.equal(node('speech-captions').checked,true,'on unless saved off');
  node('speech-captions').checked=false;let submitted;
  ctx.api=async(url,options)=>{submitted=JSON.parse(options.body);return {...response(),settings:{...response().settings,live_captions:false},captions_installed:true};};
  await ctx.onSpeechSave();assert.equal(submitted.live_captions,false);
});
test('live captions without their model say how to install it',()=>{
  const {ctx,node}=harness();
  ctx.renderSpeechSettings({...response(),captions_installed:false});
  assert.match(node('speech-captions-note').textContent,/parakeet-eou-320ms/);
  ctx.renderSpeechSettings({...response(),captions_installed:true});
  assert.doesNotMatch(node('speech-captions-note').textContent,/install/i);
  ctx.renderSpeechSettings({...response('deepgram'),captions_installed:false});
  assert.match(node('speech-captions-note').textContent,/on-device/i,'only local meetings caption');
});

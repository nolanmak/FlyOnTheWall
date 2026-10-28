const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');

function harness() {
  const nodes = new Map();
  const node = id => {
    if (!nodes.has(id)) nodes.set(id, {value:'',checked:false,hidden:false,disabled:false,textContent:''});
    return nodes.get(id);
  };
  const ctx = vm.createContext({document:{getElementById:node},window:{location:{search:'',hash:''}},URLSearchParams,localStorage:{getItem:()=>null}});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8').replace(/\nmain\(\);\s*$/, '\n'),ctx);
  return {ctx,node};
}

function response(policy,engine) {
  return {settings:{policy,cli_enabled:false,cli_kind:'claude',binary:'',acknowledged_egress:false},status:{engine,binary_resolves:false,configured_binary:'',disclosures:{anthropic:['Hosted Anthropic processing'],claude:['Hosted Claude processing'],codex:['Hosted Codex processing']}}};
}

test('a saved local-only choice displays its actual destination and setup requirement',()=>{
  const h=harness();h.ctx.renderSummarizeForm(response('local_only','local-setup-required'));
  assert.equal(h.node('sum-kind').value,'local_only');
  assert.equal(h.node('sum-enabled').checked,true);
  assert.match(h.node('sum-status').textContent,/setup required/i);
  assert.equal(h.node('sum-ack-row').hidden,true);
});

test('legacy API processing is shown enabled rather than pretending summaries are off',()=>{
  const h=harness();const body=response(undefined,'anthropic');delete body.settings.policy;
  h.ctx.renderSummarizeForm(body);
  assert.equal(h.node('sum-kind').value,'anthropic');
  assert.equal(h.node('sum-enabled').checked,true);
});

test('turning summaries off posts an explicit destination even when a hosted key exists',async()=>{
  const h=harness();h.ctx.renderSummarizeForm(response('anthropic','anthropic'));
  h.node('sum-enabled').checked=false;
  let posted;
  h.ctx.api=async(_url,options)=>{posted=JSON.parse(options.body);return response('off','off');};
  await h.ctx.onSummarizeSave();
  assert.equal(posted.policy,'off');
  assert.equal(posted.acknowledged_egress,false);
  assert.match(h.node('status').textContent,/off/i);
});

test('local-only save cannot carry stale hosted enablement or consent',async()=>{
  const h=harness();h.ctx.renderSummarizeForm(response('anthropic','anthropic'));
  h.node('sum-kind').value='local_only';h.node('sum-enabled').checked=true;h.node('sum-ack').checked=true;
  let posted;
  h.ctx.api=async(_url,options)=>{posted=JSON.parse(options.body);return response('local_only','local-setup-required');};
  await h.ctx.onSummarizeSave();
  assert.equal(posted.policy,'local_only');
  assert.equal(posted.cli_enabled,false);
  assert.equal(posted.acknowledged_egress,false);
  assert.match(h.node('status').textContent,/setup required/i);
});

test('hosted status does not say it can override every destination',()=>{
  const h=harness();
  assert.doesNotMatch(h.ctx.summarizeStatusLine(response('anthropic','anthropic').status),/wins over any/i);
});

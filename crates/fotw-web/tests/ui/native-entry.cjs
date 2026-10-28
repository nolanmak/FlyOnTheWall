const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');
function harness(hash) {
  const nodes = new Map();
  const node = id => {
    if (!nodes.has(id)) nodes.set(id, {hidden:false, open:false, textContent:'', scrolls:0, scrollIntoView(){this.scrolls++;}});
    return nodes.get(id);
  };
  const replaced = [];
  const ctx = vm.createContext({
    document:{getElementById:node}, window:{location:{search:'?t=owned-fixture',pathname:'/',hash}}, URLSearchParams,
    history:{replaceState:(_state,_title,url)=>replaced.push(url)},
    localStorage:{getItem:()=>null,setItem(){}},
    fetch:async()=>({ok:true,json:async()=>({token:'owned-session-fixture'})}),
  });
  const source = fs.readFileSync(path.join(__dirname,'../../ui/app.js'),'utf8').replace(/\nmain\(\);\s*$/, '\n');
  vm.runInContext(source,ctx);
  return {ctx,node,replaced};
}
test('native settings entry survives handoff redemption without retaining its credential', async()=>{
  const h = harness('#speech-settings');
  await vm.runInContext('redeemHandoff()',h.ctx);
  assert.deepEqual(h.replaced,['/#speech-settings']);
  assert.ok(!h.replaced[0].includes('owned-fixture'));
});
test('settings entry opens and scrolls the requested available section only',()=>{
  for (const id of ['speech-settings','sum-settings']) {
    const h = harness('#'+id);
    assert.equal(vm.runInContext('openRequestedSettings()',h.ctx),true);
    assert.equal(h.node(id).open,true);
    assert.equal(h.node(id).scrolls,1);
    assert.equal(h.node('settings-panel').open,true,'the collapsed Settings panel must open around the section');
    const other = id === 'speech-settings' ? 'sum-settings' : 'speech-settings';
    assert.equal(h.node(other).open,false);
  }
});
test('unknown anchors are stripped and do not open any settings',async()=>{
  const h = harness('#arbitrary-owned-fixture');
  await vm.runInContext('redeemHandoff()',h.ctx);
  assert.deepEqual(h.replaced,['/']);
  assert.equal(vm.runInContext('openRequestedSettings()',h.ctx),false);
  assert.equal(h.node('speech-settings').open,false);
});
test('a failed or unavailable settings section stays hidden and closed',()=>{
  const h = harness('#speech-settings');
  h.node('speech-settings').hidden=true;
  assert.equal(vm.runInContext('openRequestedSettings()',h.ctx),false);
  assert.equal(h.node('speech-settings').open,false);
  assert.equal(h.node('speech-settings').scrolls,0);
});

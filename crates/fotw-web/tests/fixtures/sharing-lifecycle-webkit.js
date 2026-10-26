// Run as an async function body in the owned, nonpersistent WebKit runner,
// against ui_preview's throwaway library. Sharing responses are synthetic;
// downloads are captured as strings. No providers, capture or clipboard.
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(check) {
  for (let i = 0; i < 200; i++) { if (check()) return; await pause(10); }
  throw Error('Fixture condition timed out');
}
await until(() => typeof token === 'function' && token());
const meetings = (await api('/api/meetings')).meetings.filter(m => m.state === 'ready');
if (meetings.length < 2) throw Error('Two synthetic ready meetings required');
const [a, b] = meetings;
const states = new Map(), loads = new Map(), downloads = [];
function state(id) {
  if (!states.has(id)) states.set(id, {revision: 1, automatic: true, document: {
    title: 'Synthetic sharing document', started_at_ms: 1700000000000,
    markdown: 'Saved document for ' + id, audience: 'Fixture reviewer',
    source_segments: 0, excerpts: [], review_notes: [], name_corrections: [],
  }});
  return states.get(id);
}
sharingRequest = async (id, request) => {
  const current = state(id);
  if (request.action === 'load') loads.set(id, (loads.get(id) || 0) + 1);
  else if (request.action === 'save') {
    if (request.revision !== current.revision) throw Error('Fixture revision mismatch');
    current.document.markdown = request.markdown; current.revision++;
  } else if (request.action === 'configure') current.automatic = request.automatic;
  else throw Error('Unexpected sharing action: ' + request.action);
  return JSON.parse(JSON.stringify(current));
};
downloadSharingDocument = editor => {
  downloads.push(sharingMarkdown(editor));
  return 'synthetic-sharing-review.md';
};
async function open(id) {
  await openMeeting(id);
  await until(() => document.querySelector('.sharing-panel textarea')?.disabled === false);
}
await open(a.id);
for (const details of document.querySelectorAll('.sharing-panel details')) details.open = true;
const draft = 'Unsaved native review 日本語 😀';
const area = document.querySelector('.sharing-panel textarea');
area.value = draft; area.dispatchEvent(new Event('input', {bubbles: true}));
await open(b.id);
showLive();
if (document.querySelector('.sharing-panel')) throw Error('Sharing panel survived live navigation');
await open(a.id);
if (document.querySelector('.sharing-panel textarea').value !== draft || loads.get(a.id) !== 1)
  throw Error('Navigation replaced the unsaved draft');
const save = [...document.querySelectorAll('.sharing-panel button')].find(n => n.textContent === 'Save & download .md');
if (!save) throw Error('Save action missing');
save.click();
await until(() => downloads.length === 1 && !document.querySelector('.sharing-panel textarea').disabled);
if (!downloads[0].includes(draft)) throw Error('Saved export lost reviewer edits');
for (let i = 0; i < 20; i++) await open(i % 2 ? a.id : b.id);
await open(a.id);
if (document.querySelector('.sharing-panel textarea').value !== draft || downloads.length !== 1)
  throw Error('Clean navigation lost saved content or repeated a download');
const scriptHashes = {};
for (const file of ['app.js', 'documents.js']) {
  const source = await (await fetch('/assets/' + file)).text();
  if (file === 'documents.js' && !source.includes('function releaseSharingDocument()'))
    throw Error('Served assets do not contain the lifecycle fix');
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(source));
  scriptHashes[file] = [...new Uint8Array(digest)].map(n => n.toString(16).padStart(2, '0')).join('');
}
for (const details of document.querySelectorAll('.sharing-panel details')) details.open = true;
document.querySelector('.sharing-panel textarea').scrollIntoView({block: 'center'});
return {cssWidth: innerWidth, draftPreserved: true, originalRevisionSaved: true,
  liveNavigationClearedSharing: true, cleanNavigationCount: 20,
  downloadIntents: downloads.length, actualDownloads: 0, scriptHashes,
  processMemoryMeasured: false, browserGcForced: false};

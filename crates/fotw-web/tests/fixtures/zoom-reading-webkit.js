// Run only against ui_preview --live-notes in an owned, nonpersistent WebKit
// window. The recorder is synthetic; this never copies to the clipboard.
let rows;
for (let i = 0; i < 150; i++) {
  try { rows = (await api('/api/meetings')).meetings; } catch {}
  if (rows?.length && document.getElementById('record')?.textContent === 'Stop') break;
  await new Promise(resolve => setTimeout(resolve, 50));
}
if (!rows?.length) throw Error('Missing synthetic library');
await openMeeting(rows[0].id);
await new Promise(resolve => setTimeout(resolve, 200));
const selectedTitle = document.querySelector('#detail h2').textContent;
const checkControl = (node, label) => {
  if (!node || node.hidden || !node.getClientRects().length) throw Error('Hidden ' + label);
  node.scrollIntoView({block: 'center'});
  const rect = node.getBoundingClientRect();
  if (rect.left < -1 || rect.right > innerWidth + 1 || rect.top < -1 || rect.bottom > innerHeight + 1 || node.scrollWidth > node.clientWidth + 1)
    throw Error('Clipped ' + label);
  node.focus();
  if (document.activeElement !== node) throw Error('Unfocusable ' + label);
};
const checkReading = () => {
  const utterance = document.querySelector('#detail .segment');
  if (!utterance) throw Error('Missing synthetic utterance');
  utterance.scrollIntoView({block: 'center'});
  const rect = utterance.getBoundingClientRect();
  let top = 0, bottom = innerHeight;
  for (let parent = utterance.parentElement; parent; parent = parent.parentElement) {
    if (['auto', 'scroll', 'hidden', 'clip'].includes(getComputedStyle(parent).overflowY)) {
      const bounds = parent.getBoundingClientRect();
      top = Math.max(top, bounds.top);
      bottom = Math.min(bottom, bounds.bottom);
    }
  }
  if (rect.top < top - 1 || rect.bottom > bottom + 1)
    throw Error('Complete utterance clipped by reading pane: ' + JSON.stringify({top: rect.top, bottom: rect.bottom, visibleTop: top, visibleBottom: bottom}));
  return {utteranceHeight: rect.height, visibleReadingHeight: bottom - top};
};
const beforeStop = checkReading();
checkControl(document.getElementById('record'), 'Stop');
checkControl(document.getElementById('return-live'), 'Return to recording');
checkControl([...document.querySelectorAll('#detail button')].find(button => button.textContent === 'Copy transcript'), 'Copy transcript');
if (document.documentElement.scrollWidth > innerWidth + 1) throw Error('Horizontal page overflow');
document.getElementById('record').click();
for (let i = 0; i < 150 && document.getElementById('record').textContent !== 'Start'; i++)
  await new Promise(resolve => setTimeout(resolve, 50));
const consent = document.getElementById('consent');
consent.checked = true;
consent.dispatchEvent(new Event('change', {bubbles: true}));
for (let i = 0; i < 100 && document.getElementById('record').disabled; i++)
  await new Promise(resolve => setTimeout(resolve, 50));
checkControl(document.getElementById('record'), 'Start');
if (document.querySelector('#detail h2').textContent !== selectedTitle) throw Error('Stop changed saved meeting selection');
if (document.documentElement.scrollWidth > innerWidth + 1) throw Error('Horizontal overflow after Stop');
const afterStop = checkReading();
return {cssWidth: innerWidth, cssHeight: innerHeight, beforeStop, afterStop, stopReachable: true, startReachable: true, returnReachable: true, copyReachable: true, savedSelectionPreserved: true};

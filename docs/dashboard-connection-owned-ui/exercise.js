const wait = async (test, label) => {
  for (let i=0;i<200;i++) { if (test()) return; await new Promise(r=>setTimeout(r,50)); }
  throw Error(label);
};
await wait(()=>el.live.textContent==='Updates connected' && el.record.textContent==='Stop','initial connected recording');
const realApi=api;
let failures=0;
api=async(path,...args)=>{
  if(path==='/api/ws-ticket' && failures++<2) throw Error('synthetic ticket outage');
  return realApi(path,...args);
};
streamSocket.close();
await wait(()=>el.live.textContent==='Updates reconnecting…','disconnect status');
const checkStop=()=>{
  const node=el.record; node.scrollIntoView({block:'center'});
  const r=node.getBoundingClientRect();
  if(node.hidden || node.disabled || node.textContent!=='Stop' || r.left<0 || r.right>innerWidth+1 || r.top<0 || r.bottom>innerHeight+1) throw Error('Stop unavailable');
  if(el.recording.hidden || el.recording.textContent!=='recording' || el.elapsed.hidden) throw Error('Recorder state erased by disconnect');
  if(document.documentElement.scrollWidth>innerWidth+1) throw Error('Horizontal overflow');
};
checkStop();
const disconnected={label:el.live.textContent,recording:el.recording.textContent,elapsed:el.elapsed.textContent,stopAvailable:true};
if(innerWidth>200) {
  await wait(()=>failures>=3 && el.live.textContent==='Updates connected','ticket retry and socket reconnect');
  checkStop();
}
return {width:innerWidth,height:innerHeight,disconnected,failedTickets:Math.min(failures,2),reconnected:el.live.textContent==='Updates connected',finalLabel:el.live.textContent,recordingPreserved:!el.recording.hidden,stopAvailable:true,horizontalOverflow:false};

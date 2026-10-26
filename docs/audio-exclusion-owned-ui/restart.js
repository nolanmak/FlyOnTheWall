const check=(value,message)=>{if(!value)throw new Error(message)};
for(let i=0;i<400&&(document.getElementById('capture-settings').hidden||!captureDraftKnown);i++)await new Promise(r=>setTimeout(r,10));
const stored=await api('/api/settings/capture');
check(stored.settings.ignore_spotify===false&&stored.settings.ignore_brave===true,'settings did not persist across owned server restart');
check(!document.getElementById('capture-spotify').checked&&document.getElementById('capture-brave').checked,'restarted real UI does not match saved preferences');
check(document.getElementById('capture-summary').textContent.includes('next recording: Brave'),'restarted summary is wrong');
document.getElementById('capture-settings').open=true;
return {authenticatedStartup:true,actualEncryptedStoreRestart:true,uiMatchesPersistedChoice:true,stored:stored.settings,scope:'Second owned server process and a fresh nonpersistent WebKit window; no user library or credentials'};

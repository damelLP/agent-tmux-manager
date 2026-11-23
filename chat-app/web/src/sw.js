const [revision, indexHash, paths] = /* @SHELL@ */ [];
const prefix = 'atm-chat-shell-', name = prefix + revision;
self.addEventListener('install', event => event.waitUntil((async () => {
  const cache = await caches.open(name);
  try {
    for (const path of paths) {
      const res = await fetch(path,{cache:'no-store',redirect:'error'});
      if (!res.ok) throw new Error('Incomplete shell');
      if (path === '/index.html') {
        const hash = new Uint8Array(await crypto.subtle.digest('SHA-256',await res.clone().arrayBuffer()));
        if (btoa(String.fromCharCode(...hash)) !== indexHash) throw new Error('Mismatched shell');
      }
      await cache.put(path,res);
    }
  } catch (error) { await caches.delete(name); throw error; }
})()));
self.addEventListener('activate', event => event.waitUntil((async () => {
  await Promise.all((await caches.keys()).filter(key=>key.startsWith(prefix)&&key!==name).map(key=>caches.delete(key)));
  await self.clients.claim();
})()));
self.addEventListener('message', event => {if(event.data==='activate') event.waitUntil(self.skipWaiting());});
self.addEventListener('fetch', event => {
  const url = new URL(event.request.url), path = url.pathname === '/' ? '/index.html' : url.pathname;
  if(event.request.method==='GET' && url.origin===self.location.origin && !url.search && paths.includes(path)) {
    event.respondWith(caches.open(name).then(cache=>cache.match(path)).then(res=>res??fetch(event.request)));
  }
});
self.addEventListener('push', event => event.waitUntil((async()=>{
  let notice;
  try {
    const res=await fetch('/notify/latest',{mode:'same-origin',cache:'no-store',redirect:'error',signal:AbortSignal.timeout(4000)});
    if(res.status===200) notice=await res.json();
  } catch { /* Tailnet may be asleep. */ }
  const t=notice?.target, route=new URLSearchParams();
  if(typeof t?.decision==='string') route.set('d',t.decision);
  else if(typeof t?.channel==='string') {
    route.set('c',t.channel);
    if(typeof t.thread==='string') route.set('t',t.thread);
  }
  const data=typeof notice?.body==='string'?route.toString():'';
  await self.registration.showNotification('ATM Chat',{body:data&&notice.body||'New activity in ATM Chat',tag:'atm-chat',renotify:true,data});
})()));
self.addEventListener('notificationclick',event=>{
  const notice=event.notification.data||'';
  event.notification.close();
  event.waitUntil((async()=>{
    const windows=await self.clients.matchAll({type:'window'});
    const app=windows.find(w=>['/','/index.html'].includes(new URL(w.url).pathname));
    if(!app) return self.clients.openWindow(notice?`/#${notice}`:'/');
    app.postMessage({notice});
    await app.focus();
  })());
});

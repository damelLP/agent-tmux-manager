const subscription='/notify/subscription';
function settings(init?:RequestInit) {
  return fetch(subscription,{cache:'no-store',mode:'same-origin',redirect:'error',signal:AbortSignal.timeout(5000),...init});
}

async function bounded<T>(operation:Promise<T>) {
  let timer:number|undefined;
  try {return await Promise.race([operation,new Promise<never>((_,reject)=>{timer=window.setTimeout(reject,8000);})]);}
  finally {window.clearTimeout(timer);}
}

function notifications(reg:ServiceWorkerRegistration) {
  const button=document.querySelector<HTMLButtonElement>('#notifications');
  if(!button||!('Notification' in window)||!reg.pushManager) return;
  let config:{key:string;endpoint:string|null}|undefined, local:PushSubscription|null, busy=false;
  const matching=()=>!!local&&local.endpoint===config?.endpoint;
  const refresh=async(toggle=false)=>{
    if(busy) return;
    busy=button.disabled=true;
    try {
      if(toggle&&config) {
        const on=!matching();
        if(!on||Notification.permission==='granted'||await Notification.requestPermission()==='granted') {
          // A bridge-pruned endpoint can still be registered in the browser.
          if(on&&local) await bounded(local.unsubscribe());
          const sub=on?await bounded(reg.pushManager.subscribe({userVisibleOnly:true,applicationServerKey:config.key})):local;
          if(sub) {
            const r=await settings({method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({endpoint:sub.endpoint,on})});
            if(!r.ok) throw Error();
            if(!on) await bounded(sub.unsubscribe());
          }
        }
      }
      const r=await settings();
      if(!r.ok) throw Error();
      config=await r.json(); local=await reg.pushManager.getSubscription();button.hidden=false;
      const on=matching()&&Notification.permission==='granted';
      button.textContent=`Notifications: ${on?'On':Notification.permission==='denied'?'Blocked':'Off'}`;
    } catch {config=undefined;button.textContent='Notifications unavailable';}
    finally {busy=false;button.disabled=!matching()&&Notification.permission==='denied';}
  };
  button.onclick=()=>void refresh(true);
  addEventListener('online',()=>void refresh());
  document.addEventListener('visibilitychange',()=>{if(!document.hidden) void refresh();});
  void refresh();
}

export function registerPwa(canReload:()=>boolean,prepareReload:()=>boolean) {
  if(!import.meta.env.PROD || !('serviceWorker' in navigator)) return;
  const banner=document.getElementById('update-banner'), reload=document.getElementById('update-reload');
  if(!banner||!(reload instanceof HTMLButtonElement)) return;
  const sw=navigator.serviceWorker;
  void sw.register('/sw.js',{updateViaCache:'none'}).then(reg=>{
    let accepting=false;
    const reflect=()=>{banner.hidden=!reg.waiting||!sw.controller;reload.disabled=accepting;};
    function check() {void reg.update().catch(()=>{});reflect();}
    reg.addEventListener('updatefound',()=>reg.installing?.addEventListener('statechange',reflect));
    addEventListener('online',check);
    document.addEventListener('visibilitychange',()=>{if(document.visibilityState==='visible') check();});
    sw.addEventListener('controllerchange',()=>{if(accepting){accepting=false;location.reload();}else reflect();});
    reload.addEventListener('click',()=>{
      if(!reg.waiting||accepting||!canReload()||!prepareReload()) return;
      accepting=true;reload.disabled=true;reg.waiting.postMessage('activate');
    });
    check();
    void sw.ready.then(notifications);
  }).catch(()=>{});
}

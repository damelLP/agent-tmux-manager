import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, frame, launch, openChannel, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const options={serviceWorkers:'allow'};
const phoneViewport={viewport:{width:360,height:787},isMobile:true};
const endpoint='https://fcm.googleapis.com/wp/fixture-phone', key='B'+'A'.repeat(86);
const on='#notifications:not([disabled])';
const you=page=>page.getByRole('button',{name:'You Settings',exact:true});
const inPane=(page,name)=>dom(page,`#app[data-pane="${name}"]`);
const depth=page=>page.evaluate(()=>history.state?.depth);
const atRoot=page=>page.evaluate(()=>history.state?.depth===0||new Promise(done=>addEventListener('popstate',()=>done(),{once:true})));
const buttonText=(page,text,disabled=false)=>page.evaluate(({text,disabled})=>new Promise(done=>{
  const button=document.getElementById('notifications'), observer=new MutationObserver(check);
  function check() {
    if(button.textContent===text&&button.disabled===disabled&&!button.hidden) {observer.disconnect();done();}
  }
  observer.observe(button,{characterData:true,childList:true,attributes:true,subtree:true});check();
}),{text,disabled});

async function enterSettings(page) {
  expect(await you(page).count()).toBe(1);
  await you(page).press('Enter');
  expect(await page.locator('#app').getAttribute('data-pane')).toBe('settings');
}

async function activeWorker(page,origin) {
  const born=page.context().waitForEvent('serviceworker');
  await page.goto(origin);
  await page.evaluate(async()=>{
    await navigator.serviceWorker.ready;
    if(!navigator.serviceWorker.controller) await new Promise(done=>navigator.serviceWorker.addEventListener('controllerchange',done,{once:true}));
  });
  return born;
}

function routes(server,{status=()=>200, preview=()=>null, permissionWrites=[], endpointOn=null, subscriptionStatus=()=>200}={}) {
  let saved=endpointOn;
  server.control.http=(req,res)=>{
    if(req.url==='/notify/latest') {
      const body=preview();
      res.writeHead(body?status():204,{'content-type':'application/json','cache-control':'no-store'}).end(body?JSON.stringify(body):'');
      return true;
    }
    if(req.url!=='/notify/subscription') return false;
    if(req.method==='GET') res.writeHead(subscriptionStatus(),{'content-type':'application/json','cache-control':'no-store'}).end(JSON.stringify({key,endpoint:saved}));
    else {
      let data='';
      req.on('data',part=>data+=part);
      req.on('end',()=>{
        const change=JSON.parse(data);permissionWrites.push({origin:req.headers.origin,...change});
        saved=change.on?change.endpoint:null;res.writeHead(204).end();
      });
    }
    return true;
  };
  return ()=>{saved=null;};
}

function stubPhone(page) {
  return page.context().addInitScript(ep=>{
    window.pushCalls=[];
    Object.defineProperties(Notification,{
      permission:{get:()=>sessionStorage.noticePermission??'default'},
      requestPermission:{value:async()=>{window.pushCalls.push('permission');sessionStorage.noticePermission='granted';return 'granted';}},
    });
    PushManager.prototype.getSubscription=async()=>sessionStorage.phone?sub():null;
    PushManager.prototype.subscribe=async opts=>{
      window.pushCalls.push(opts);
      if(!sessionStorage.phone) {
        sessionStorage.phone=sessionStorage.newPhone?ep+'-renewed':ep;
        sessionStorage.newPhone='yes';
      }
      return sub();
    };
    function sub(){return {endpoint:sessionStorage.phone,unsubscribe:async()=>{window.pushCalls.push('unsubscribe');sessionStorage.removeItem('phone');return true;}};}
  },endpoint);
}

test.each(['keyboard','touch'])('You opens the only notification control; Back restores Home and focus (%s)',mode=>withChat(browser,async(page,server)=>{
  routes(server);
  await stubPhone(page);
  const prior=`${server.origin}/before-chat`;
  await page.route(prior,r=>r.fulfill({contentType:'text/html',body:'<title>Before chat</title>'}));
  await page.goto(prior);
  await activeWorker(page,server.origin);
  await dom(page,'#app[data-pane="home"][data-phase="live"] #channels [data-channel="demo"]');
  await buttonText(page,'Notifications: Off');
  const root=await depth(page);
  const selected=await page.evaluate(()=>history.state.view.channel);
  const toggle=page.locator('#settings #notifications');
  expect(await page.locator('#side-log #notifications').count()).toBe(0);
  expect(await page.locator('#notifications').count()).toBe(1);
  expect(await toggle.isVisible()).toBe(false);
  expect(await you(page).count()).toBe(1);
  const back=page.getByRole('button',{name:'Back',exact:true});
  if(mode==='keyboard') {
    await page.keyboard.press('Tab');
    expect(await you(page).evaluate(el=>el===document.activeElement)).toBe(true);
    await page.keyboard.press('Enter');
  } else await you(page).tap();
  expect(await depth(page)).toBe(root+1);
  expect(await toggle.isVisible()).toBe(true);
  expect((await toggle.boundingBox())?.height).toBeGreaterThanOrEqual(44);
  expect(await back.evaluate(el=>el===document.activeElement)).toBe(true);
  expect(await page.evaluate(()=>[history.state.view.pane,history.state.view.channel])).toEqual(['settings',selected]);
  if(mode==='keyboard') {
    await page.keyboard.press('Tab');
    expect(await toggle.evaluate(el=>el===document.activeElement)).toBe(true);
    await page.keyboard.press('Space');
  } else await toggle.tap();
  await buttonText(page,'Notifications: On');
  if(mode==='keyboard') await back.press('Space'); else await back.tap();
  await inPane(page,'home');
  expect(await depth(page)).toBe(0);
  expect(await you(page).evaluate(el=>el===document.activeElement)).toBe(true);
  expect(await page.evaluate(()=>history.state.view.channel)).toBe(selected);
  await page.goForward();await inPane(page,'settings');
  expect(await back.evaluate(el=>el===document.activeElement)).toBe(true);
  const documents=server.http.filter(r=>r.url==='/').length;
  await page.goBack();await inPane(page,'home');
  expect(await depth(page)).toBe(0);
  expect(await toggle.isVisible()).toBe(false);
  expect(server.http.filter(r=>r.url==='/')).toHaveLength(documents);
  await page.goBack();expect(page.url()).toBe(prior);
},{...options,...phoneViewport,hasTouch:mode==='touch'}));

test('You stays at the top of Home when a bookmark places the list below it',()=>withChat(browser,async(page,server)=>{
  await page.goto(server.origin);
  await dom(page,'#app[data-phase="live"] #channels [data-channel="demo"]');
  const side=page.locator('#side-log');
  const scroll=await side.evaluate(el=>{el.scrollTop=el.scrollHeight;return el.scrollTop;});
  expect(scroll).toBeGreaterThan(48);
  await frame(page);
  const atTop=await you(page).evaluate(el=>{
    const {left,top,width,height}=el.getBoundingClientRect();
    const {bottom:header}=el.closest('.side').querySelector('header').getBoundingClientRect();
    return top===header&&el.contains(document.elementFromPoint(left+width/2,top+height/2));
  });
  expect(atTop).toBe(true);
  await you(page).tap();await inPane(page,'settings');
  await page.getByRole('button',{name:'Back',exact:true}).tap();await inPane(page,'home');
  expect(await side.evaluate(el=>el.scrollTop)).toBe(scroll);
},{...phoneViewport,hasTouch:true}));

test('the visible-document and online events update the mounted control even on Home',()=>withChat(browser,async(page,server)=>{
  const writes=[];let status=200;
  const expire=routes(server,{permissionWrites:writes,endpointOn:endpoint,subscriptionStatus:()=>status});
  await stubPhone(page);
  await page.context().addInitScript(ep=>{sessionStorage.phone=ep;sessionStorage.noticePermission='granted';},endpoint);
  await activeWorker(page,server.origin);
  await buttonText(page,'Notifications: On');
  await enterSettings(page);
  await page.locator('#notifications').evaluate(node=>{window.originalControl=node;});
  await page.goBack();await inPane(page,'home');
  expect(await page.locator('#notifications').isVisible()).toBe(false);
  expire();
  await page.evaluate(()=>dispatchEvent(new Event('online')));
  await buttonText(page,'Notifications: Off');
  await page.evaluate(()=>{sessionStorage.noticePermission='denied';document.dispatchEvent(new Event('visibilitychange'));});
  await buttonText(page,'Notifications: Blocked',true);
  status=503;
  await page.evaluate(()=>{sessionStorage.noticePermission='granted';document.dispatchEvent(new Event('visibilitychange'));});
  await buttonText(page,'Notifications unavailable');
  await enterSettings(page);
  expect(await page.locator('#notifications:visible').evaluate(node=>node===window.originalControl)).toBe(true);
  expect(await page.evaluate(()=>window.pushCalls)).toEqual([]);
  expect(writes).toEqual([]);
},{...options,...phoneViewport}));

test('Settings hides the conversation panes and cannot mark an arriving message seen',()=>withChat(browser,async(page,server)=>{
  await page.addInitScript(()=>{
    const send=WebSocket.prototype.send;
    window.seen=[];
    WebSocket.prototype.send=function(data) {
      const frame=JSON.parse(data);
      if(frame.method==='channel.seen') window.seen.push(frame.params);
      return send.call(this,data);
    };
  });
  await page.goto(server.origin);
  await dom(page,'#app[data-phase="live"] #channels [data-channel="demo"]');
  await enterSettings(page);
  for(const width of [360,2240]) {
    await page.setViewportSize({width,height:787});await frame(page);
    for(const id of ['#side-log','#channel-log','#thread-log','#needs-wide']) expect(await page.locator(id).isVisible()).toBe(false);
    expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBe(width);
  }
  const arriving=server.fake.inject('demo','Unseen while looking at Settings',{woke:['owner']});
  await waitForPost(page,arriving.id);await frame(page);
  expect(await page.evaluate(()=>window.seen)).toEqual([]);
},{...phoneViewport,hasTouch:true}));

test('identity refusal returns to Home and Forward cannot revive Settings',()=>withChat(browser,async(page,server)=>{
  await page.goto(server.origin);
  await dom(page,'#app[data-phase="live"] #channels [data-channel="demo"]');
  await enterSettings(page);
  const run=await page.evaluate(()=>history.state.run);
  server.control.health='identity';server.control.refusal='identity';server.disconnect();
  await dom(page,'#app[data-phase="identity"][data-pane="home"]');await atRoot(page);
  expect(await page.evaluate(()=>history.state.run)).not.toBe(run);
  expect(await page.locator('#open-settings').isVisible()).toBe(false);
  await page.goForward();await atRoot(page);
  expect(await page.locator('#app').getAttribute('data-pane')).toBe('home');
},phoneViewport));

test('Enable asks once, replaces an expired endpoint, and Disable clears both ends',()=>withChat(browser,async(page,server)=>{
  const writes=[], expire=routes(server,{permissionWrites:writes});
  await stubPhone(page);
  await activeWorker(page,server.origin);
  expect(await page.locator('#notifications').count()).toBe(1);
  await buttonText(page,'Notifications: Off');
  expect(await page.evaluate(()=>window.pushCalls)).toEqual([]);
  await enterSettings(page);
  await page.locator(on).click();
  await buttonText(page,'Notifications: On');
  expect(await page.evaluate(()=>window.pushCalls)).toEqual(['permission',{userVisibleOnly:true,applicationServerKey:key}]);
  // An ordinary reload resets navigation but reads both endpoints without another prompt.
  await page.reload();await inPane(page,'home');await atRoot(page);
  await buttonText(page,'Notifications: On');
  expect(await page.evaluate(()=>window.pushCalls)).toEqual([]);
  await enterSettings(page);
  expire(); // Bridge cleared the slot on 410; the browser still holds the old URL.
  await page.evaluate(()=>dispatchEvent(new Event('online')));
  await buttonText(page,'Notifications: Off');
  await page.locator(on).click();
  await buttonText(page,'Notifications: On');
  await page.locator(on).click();
  await buttonText(page,'Notifications: Off');
  expect(await page.evaluate(()=>window.pushCalls.at(-1))).toBe('unsubscribe');
  expect(writes).toEqual([endpoint,endpoint+'-renewed',endpoint+'-renewed'].map((endpoint,i)=>({origin:server.origin,endpoint,on:i<2})));
},options));

for(const operation of ['subscribe','unsubscribe']) {
  test(`a stalled ${operation} releases the notification control`,()=>withChat(browser,async(page,server)=>{
    routes(server,{endpointOn:operation==='unsubscribe'?endpoint:null});
    await page.clock.install();
    await page.context().addInitScript(({operation,endpoint})=>{
      let started;
      window.started=new Promise(r=>{started=r;});
      const stall=()=>{started();return new Promise(()=>{});};
      Object.defineProperty(Notification,'permission',{get:()=> 'granted'});
      PushManager.prototype.getSubscription=async()=>operation==='unsubscribe'?{endpoint,unsubscribe:stall}:null;
      PushManager.prototype.subscribe=stall;
    },{operation,endpoint});
    await activeWorker(page,server.origin);
    await buttonText(page,operation==='subscribe'?'Notifications: Off':'Notifications: On');
    await enterSettings(page);
    await page.locator(on).click();
    await page.evaluate(()=>window.started);
    await page.clock.runFor(10_000);
    const button=page.locator('#notifications');
    expect(await button.isEnabled()).toBe(true);
    expect(await button.textContent()).toBe('Notifications unavailable');
  },options));
}

async function deliver(worker,type,{notice=null,noWindows=false}={}) {
  return worker.evaluate(async({type,notice,noWindows})=>{
    const r=self.registration, c=self.clients;
    const original=[r.showNotification,c.matchAll,c.openWindow];
    let wait, shown, focused=false, opened, closed=false;
    r.showNotification=async(title,o)=>{shown={title,...o};};
    c.openWindow=async url=>{opened=new URL(url,self.location.origin).href;};
    c.matchAll=async opts=>noWindows?[]:(await original[1].call(c,opts)).map(w=>({url:w.url,postMessage:v=>w.postMessage(v),focus:async()=>{focused=true;}}));
    const e=new Event(type);
    Object.assign(e,{data:null,notification:{data:notice,close:()=>{closed=true;}},waitUntil:p=>{wait=p;}});
    try {
      self.dispatchEvent(e);
      if(!wait) throw Error(`worker did not extend ${type}`);
      await wait;
      return {shown,focused,opened,closed};
    } finally {[r.showNotification,c.matchAll,c.openWindow]=original;}
  },{type,notice,noWindows});
}

test('an empty push pulls a private preview, focuses an existing app, and replays a thread at cold start',()=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Notification thread root');
  const reply=server.fake.inject('demo','Reply arrived on the phone',{thread:root.id});
  routes(server,{preview:()=>({body:'Reply arrived on the phone',target:{channel:'demo',thread:root.id}})});
  const worker=await activeWorker(page,server.origin);
  const push=await deliver(worker,'push');
  expect(push.shown).toMatchObject({title:'ATM Chat',body:'Reply arrived on the phone',tag:'atm-chat',renotify:true});
  expect(server.http.filter(r=>r.url==='/notify/latest')).toEqual([{method:'GET',url:'/notify/latest'}]);
  await enterSettings(page);
  const clicked=await deliver(worker,'notificationclick',{notice:push.shown.data});
  expect(clicked).toMatchObject({focused:true,closed:true});
  await dom(page,'#app[data-pane="thread"]');
  await waitForPost(page,reply.id,'replies');
  const cold=await deliver(worker,'notificationclick',{notice:push.shown.data,noWindows:true});
  expect(cold.opened).toBe(`${server.origin}/#c=demo&t=${root.id}`);
  const fresh=await page.context().newPage();
  await fresh.goto(cold.opened);
  await dom(fresh,'#app[data-pane="thread"]');
  await waitForPost(fresh,reply.id,'replies');
},options));

test('a provisional-decision tap focuses the card; unreachable or refused preview opens Home without losing a draft',()=>withChat(browser,async(page,server)=>{
  let status=200;
  const d=server.fake.ask('Review this provisional decision',true);
  routes(server,{status:()=>status,preview:()=>({body:'Review this provisional decision',target:{decision:d}})});
  const worker=await activeWorker(page,server.origin);
  await openChannel(page,server);
  const input=page.locator('#channel-form textarea');
  await input.fill('unsent draft');
  const p=await deliver(worker,'push');
  await deliver(worker,'notificationclick',{notice:p.shown.data});
  await dom(page,`#app[data-pane="home"] article[data-scroll-id="${d}"]:focus`);
  await page.locator('#channels [data-channel="demo"]').click();
  expect(await input.inputValue()).toBe('unsent draft');
  status=403;
  const denied=await deliver(worker,'push');
  expect(denied.shown).toMatchObject({body:'New activity in ATM Chat',tag:'atm-chat'});
  const cold=await deliver(worker,'notificationclick',{notice:denied.shown.data,noWindows:true});
  expect(cold.opened).toBe(`${server.origin}/`);
  await deliver(worker,'notificationclick',{notice:denied.shown.data});
  await dom(page,'#app[data-pane="home"]');
},options));

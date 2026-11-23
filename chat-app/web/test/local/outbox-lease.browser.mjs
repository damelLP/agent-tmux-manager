import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, launch, openChannel, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const key='atm-chat-outbox-1';
const enter=async(page,body)=>{
  await page.locator('#channel-form textarea').fill(body);
  await page.locator('#channel-form button[type=submit]').click();
};
const visible=()=>Object.defineProperty(document,'visibilityState',{get:()=>window.fixtureVisibility??'visible'});
const holder=page=>page.evaluate(async name=>(await navigator.locks.query()).held.find(lock=>lock.name===name)?.clientId,key);

test('an identity denial in a waiting context invalidates the shared queue and stops the follower',()=>withChat(browser,async(owner,server)=>{
  const context=owner.context(), blocked=Promise.withResolvers(), admitted=Promise.withResolvers(), sent=[];
  await context.addInitScript(visible);
  await owner.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer();let head;
    upstream.onMessage(raw=>{
      const f=JSON.parse(String(raw));if(head&&f.id===head.id) admitted.resolve(f.result.post_id);
      client.send(raw);
    });
    client.onMessage(raw=>{
      const f=JSON.parse(String(raw));
      if(f.method==='channel.post') {
        sent.push(f.params.body);
        if(!head) {head=f;blocked.resolve(()=>upstream.send(raw));return;}
      }
      upstream.send(raw);
    });
  });
  await openChannel(owner,server);const writer=await holder(owner);expect(writer).toBeTruthy();
  await enter(owner,'Already in flight');const admit=await blocked.promise;
  await enter(owner,'Unsent text to discard');
  expect(await owner.evaluate(k=>JSON.parse(localStorage.getItem(k)).length,key)).toBe(2);

  server.control.refusal='identity';
  const denied=await context.newPage();await denied.goto(server.origin);await dom(denied,'#app[data-phase=identity]');
  expect(await holder(owner)).toBe(writer);
  expect(await denied.evaluate(k=>localStorage.getItem(k)===null||localStorage.getItem(k+'-denied')==='1',key)).toBe(true);
  await owner.evaluate(k=>new Promise(done=>{
    const observer=new MutationObserver(check);
    function check(){if(!document.querySelector('#posts .outbox')&&localStorage.getItem(k)===null){observer.disconnect();done();}}
    observer.observe(document,{childList:true,subtree:true});check();
  }),key);
  admit();await waitForPost(owner,await admitted.promise);
  expect(sent).toEqual(['Already in flight']);
},{viewport:{width:840,height:760}}));

test('warns when a tab cannot clear the queue and retries when storage returns',()=>withChat(browser,async(owner,server)=>{
  const context=owner.context(), held=Promise.withResolvers();
  await context.addInitScript(visible);
  await owner.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer();upstream.onMessage(raw=>client.send(raw));
    client.onMessage(raw=>{
      if(JSON.parse(String(raw)).method==='channel.post') held.resolve();
      else upstream.send(raw);
    });
  });
  await openChannel(owner,server);await enter(owner,'A note awaiting its result');await held.promise;
  server.control.refusal='identity';
  const denied=await context.newPage();
  await denied.addInitScript(k=>{
    const put=Storage.prototype.setItem;
    window.fixtureStorageUnavailable=true;
    Storage.prototype.setItem=function(key,value){
      if(key===k&&window.fixtureStorageUnavailable) throw new DOMException('Fixture storage is unavailable','SecurityError');
      return put.call(this,key,value);
    };
  },key+'-denied');
  await denied.goto(server.origin);await dom(denied,'#app[data-phase=identity]');
  const message="Couldn't clear unsent messages on this device";
  expect(await denied.getByText(message,{exact:true}).isVisible()).toBe(true);
  await owner.evaluate(k=>new Promise(done=>{
    const mo=new MutationObserver(check);
    function check(){if(!document.querySelector('#posts .outbox')&&localStorage.getItem(k)===null){mo.disconnect();done();}}
    mo.observe(document,{childList:true,subtree:true});check();
  }),key);
  await denied.evaluate(()=>{window.fixtureStorageUnavailable=false;dispatchEvent(new Event('online'));});
  expect(await denied.getByText(message,{exact:true}).isVisible()).toBe(false);
}));

test('transfers a real Web Lock only after the old live request settles and replays its exact key',()=>withChat(browser,async(owner,server)=>{
  const context=owner.context(), lost=Promise.withResolvers(), writes=[], follower=Promise.withResolvers();
  const headBody='Committed with the receipt still held', nextBody='A follower in the same channel';
  await context.addInitScript(visible);
  await owner.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer();let head;
    client.onMessage(raw=>{
      const f=JSON.parse(String(raw));if(f.method==='channel.post') head=f;
      upstream.send(raw);
    });
    upstream.onMessage(raw=>{
      const f=JSON.parse(String(raw));
      if(head&&f.kind==='channel.posted'&&f.body?.client_key===head.params.client_key) return;
      if(head&&f.id===head.id) {
        lost.resolve({request:head.params,id:f.result.post_id,cut:()=>{void client.close();void upstream.close();}});
      } else client.send(raw);
    });
  });
  await openChannel(owner,server);await enter(owner,headBody);const first=await lost.promise;
  await enter(owner,nextBody);
  const old=await holder(owner);expect(old).toBeTruthy();
  const next=await context.newPage();
  await next.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer();let last;
    client.onMessage(raw=>{
      const f=JSON.parse(String(raw));if(f.method==='channel.post') {writes.push(f.params);if(f.params.body===nextBody) last=f.id;}
      upstream.send(raw);
    });
    upstream.onMessage(raw=>{const f=JSON.parse(String(raw));if(last&&f.id===last) follower.resolve(f.result.post_id);client.send(raw);});
  });
  await openChannel(next,server);
  await enter(next,'Do not clear this draft in the waiting window');
  expect(await next.locator('#channel-form textarea').inputValue()).toContain('waiting window');
  expect(writes).toEqual([]);
  await owner.evaluate(()=>{window.fixtureVisibility='hidden';document.dispatchEvent(new Event('visibilitychange'));});
  expect(await holder(owner)).toBe(old);
  first.cut();
  const id=await follower.promise;await waitForPost(next,id);
  expect(await holder(next)).not.toBe(old);
  expect(writes.map(p=>p.body)).toEqual([headBody,nextBody]);
  expect(writes.at(0)).toEqual(first.request);
  await waitForPost(next,first.id);
  expect(await next.locator('#posts [data-post-id] .body').filter({hasText:headBody}).count()).toBe(1);
  expect(await next.locator('#posts .outbox').count()).toBe(0);
}));

test('a failed row action explains that it cannot be changed in the non-writer tab',()=>withChat(browser,async(owner,server)=>{
  const context=owner.context(), refused=Promise.withResolvers();
  await context.addInitScript(visible);
  await owner.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer();upstream.onMessage(raw=>client.send(raw));
    client.onMessage(raw=>{
      const f=JSON.parse(String(raw));
      if(f.method==='channel.post') {client.send(JSON.stringify({type:'response',id:f.id,error:{code:'refused',message:'Fixture'}}));refused.resolve();}
      else upstream.send(raw);
    });
  });
  await openChannel(owner,server);await enter(owner,'A shared failed row');await refused.promise;
  const next=await context.newPage();await openChannel(next,server);
  const row=next.locator('#posts .outbox');
  await row.locator('.message-trigger').press('Enter');
  const actions=next.locator('#message-actions');
  await actions.getByRole('button',{name:'Delete',exact:true}).press('Enter');
  expect(await row.count()).toBe(1);
  expect(await actions.locator('[role=status]').allTextContents()).toEqual(["Couldn't change the outbox in this tab."]);
}));

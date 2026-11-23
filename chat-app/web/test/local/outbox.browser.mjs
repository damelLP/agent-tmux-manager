import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { afterAll, beforeAll, expect, test } from 'vitest';
import { rig } from './rig.mjs';

let browser, server;
beforeAll(async()=>{
  const {chromium}=await import(pathToFileURL(process.env.ATM_CHAT_TEST_PLAYWRIGHT));
  server=await rig(resolve(process.env.ATM_CHAT_TEST_DIST ?? 'dist'));
  browser=await chromium.launch({args:['--disable-background-networking','--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1']});
});
afterAll(async()=>{try {await browser?.close();} finally {await server?.close();}});

const waitFor=(page,selector)=>page.evaluate(selector=>new Promise(done=>{
  const observer=new MutationObserver(check);
  function check(){if(document.querySelector(selector)) {observer.disconnect();done();}}
  observer.observe(document,{attributes:true,childList:true,subtree:true});check();
}),selector);
const channel='#app[data-phase=live] #channels [data-channel="demo"]';

// A request is stopped at the socket boundary, not by a timed server delay.
test('shows pending text before the network accepts it and reconciles after reload',async()=>{
  const context=await browser.newContext({viewport:{width:390,height:844}});
  const first=Promise.withResolvers(), replay=Promise.withResolvers();
  const seen=[], real='Queued before a receipt', later='Queued while it waits';
  try {
    const page=await context.newPage();
    // A loopback-only network namespace is otherwise reported as offline by Chromium.
    await page.addInitScript(()=>Object.defineProperty(navigator,'onLine',{configurable:true,value:true}));
    await page.routeWebSocket('**/ws',client=>{
      const upstream=client.connectToServer();
      upstream.onMessage(raw=>client.send(raw));
      upstream.onClose((code,reason)=>{void client.close({code:code===1006||code===1005?undefined:code,reason});});
      client.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.method==='channel.post') {
          seen.push(f.params);
          if(seen.length===1) {first.resolve(f.params);return;}
          if(seen.length===2) replay.resolve(f.params);
        }
        upstream.send(raw);
      });
    });
    await page.goto(server.origin);await waitFor(page,channel);await page.locator(channel).click();
    const input=page.locator('#channel-form textarea'), send=page.locator('#channel-form button[type=submit]');
    await input.fill(real);await send.click();
    expect(await input.inputValue()).toBe('');
    const initial=await first.promise;
    const row=page.locator('#posts .msg').filter({hasText:real});
    expect(await row.textContent()).toContain('Sending…');
    expect(await row.getAttribute('data-post-id')).toBeNull();
    await input.fill(later);await send.click();
    expect(await input.inputValue()).toBe('');expect(seen).toEqual([initial]);
    expect(await page.locator('#posts .outbox .body').allTextContents()).toEqual([real,later]);
    const owner=()=>page.evaluate(async()=> (await navigator.locks.query()).held.find(l=>l.name==='atm-chat-outbox-1')?.clientId);
    const firstOwner=await owner();expect(firstOwner).toBeTruthy();
    await page.reload();await waitFor(page,channel);await page.locator(channel).click();
    expect(await replay.promise).toEqual(initial);
    expect(await owner()).toBeTruthy();expect(await owner()).not.toBe(firstOwner);
    const bodies=await page.evaluate(([a,b])=>new Promise(done=>{
      const observer=new MutationObserver(check);
      function check(){
        const rows=[...document.querySelectorAll('#posts .msg[data-post-id] .body')].map(node=>node.textContent);
        if(!rows.includes(a)||!rows.includes(b)) return;
        observer.disconnect();done(rows.filter(text=>text===a||text===b));
      }
      observer.observe(document.querySelector('#posts'),{childList:true,subtree:true,attributes:true});check();
    }),[real,later]);
    expect(bodies).toEqual([real,later]);
    expect(seen.map(p=>p.body)).toEqual([real,real,later]);
    expect(await row.count()).toBe(1);
  } finally {await context.close();}
},45_000);

test('a file can post before held text and its manual Retry keeps the accessible name and key',async()=>{
  const context=await browser.newContext({viewport:{width:840,height:760}});
  const held=Promise.withResolvers(), fileReceipt=Promise.withResolvers(), textReceipt=Promise.withResolvers(), writes=[];
  try {
    const page=await context.newPage();
    await page.addInitScript(()=>{
      Object.defineProperty(navigator,'onLine',{value:true});
      // Older engines don't reflect `ariaLabel` to the DOM attribute.
      Object.defineProperty(HTMLButtonElement.prototype,'ariaLabel',{configurable:true,writable:true});
    });
    await page.route('**/upload?*',r=>r.fulfill({status:201,json:{path:'/fixture/note.txt',size:4}}));
    await page.routeWebSocket('**/ws',client=>{
      const upstream=client.connectToServer();let textId, fileId;
      client.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.method==='channel.post') {
          writes.push(f.params);
          if(!f.params.attachments) {textId=f.id;held.resolve(()=>upstream.send(raw));return;}
          if(!fileId) {
            fileId=f.id;client.send(JSON.stringify({type:'response',id:f.id,error:{code:'timeout',message:'Fixture uncertain result'}}));return;
          }
          fileId=f.id;
        }
        upstream.send(raw);
      });
      upstream.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.id&&f.id===fileId) fileReceipt.resolve(f.result.post_id);
        if(f.id&&f.id===textId) textReceipt.resolve(f.result.post_id);
        client.send(raw);
      });
    });
    await page.goto(server.origin);await waitFor(page,channel);await page.locator(channel).click();
    const form=page.locator('#channel-form'), draft=form.locator('textarea'), send=form.locator('button[type=submit]');
    await draft.fill('Text waiting on delivery');await send.click();const release=await held.promise;
    await form.getByRole('button',{name:'Attach',exact:true}).click();
    const choose=page.waitForEvent('filechooser');await form.getByRole('button',{name:'Files',exact:true}).click();
    await (await choose).setFiles({name:'note.txt',mimeType:'text/plain',buffer:Buffer.from('note')});
    await waitFor(page,'#channel-form .picks .file-chip');await send.click();
    await waitFor(page,'#channel-form button[data-retry]');
    expect(await send.getAttribute('aria-label')).toBe('Retry');
    const retry=form.getByRole('button',{name:'Retry',exact:true});expect(await retry.isEnabled()).toBe(true);
    await retry.click();const earlier=await fileReceipt.promise;
    await waitFor(page,`#posts [data-post-id="${earlier}"]`);
    expect(await page.locator('#posts .outbox .post-status').allTextContents()).toEqual(['Sending…']);
    expect(writes.map(p=>p.attachments??null)).toEqual([null,['/fixture/note.txt'],['/fixture/note.txt']]);
    expect(writes.at(-1)).toEqual(writes.at(-2));
    release();const later=await textReceipt.promise;await waitFor(page,`#posts [data-post-id="${later}"]`);
    expect(await page.locator(`#posts :is([data-post-id="${earlier}"],[data-post-id="${later}"])`).evaluateAll(rows=>rows.map(r=>r.dataset.postId))).toEqual([earlier,later]);
  } finally {await context.close();}
},45_000);

test('shows a distinct committed owner row while the same-body send is held',async()=>{
  const context=await browser.newContext({viewport:{width:840,height:760}});
  const entered=Promise.withResolvers(), release=Promise.withResolvers(), confirmed=Promise.withResolvers();
  const body='Two sends with exactly the same text';
  try {
    const page=await context.newPage();
    await page.addInitScript(()=>Object.defineProperty(navigator,'onLine',{value:true}));
    let request;
    await page.routeWebSocket('**/ws',client=>{
      const upstream=client.connectToServer();
      client.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.method==='channel.post'&&f.params.body===body) {request=f;entered.resolve();void release.promise.then(()=>upstream.send(raw));return;}
        upstream.send(raw);
      });
      upstream.onMessage(raw=>{const f=JSON.parse(String(raw));if(request&&f.id===request.id) confirmed.resolve(f.result.post_id);client.send(raw);});
    });
    await page.goto(server.origin);await waitFor(page,channel);await page.locator(channel).click();
    await page.locator('#channel-form textarea').fill(body);await page.locator('#channel-form button[type=submit]').click();await entered.promise;
    const other=server.fake.call('channel.post',{channel:'demo',body,client_key:'a-second-owner-client'});
    const fence=server.fake.inject('demo','Independent history fence');
    await waitFor(page,`#posts [data-post-id="${fence.id}"]`);
    expect(await page.locator(`#posts [data-post-id="${other.post_id}"] .body`).allTextContents()).toEqual([body]);
    expect(await page.locator('#posts .outbox').count()).toBe(1);
    release.resolve();const id=await confirmed.promise;
    expect(id).not.toBe(other.post_id);await waitFor(page,`#posts [data-post-id="${id}"]`);
    expect(await page.locator('#posts [data-post-id] .body').filter({hasText:body}).count()).toBe(2);
  } finally {await context.close();}
},45_000);

test('keeps the checked target through disconnect and a failed save, then resets it on acceptance',async()=>{
  const context=await browser.newContext({viewport:{width:840,height:760}});
  try {
    const page=await context.newPage(), root=server.fake.inject('demo','Thread with a shared draft');
    server.fake.inject('demo','A prior reply',{thread:root.id});
    await page.addInitScript(()=>{
      window.fixtureOnline=true;
      Object.defineProperty(navigator,'onLine',{get:()=>window.fixtureOnline});
      const store=Storage.prototype.setItem;
      Storage.prototype.setItem=function(key,value) {
        if(key==='atm-chat-outbox-1'&&window.fixtureDenyStore) throw new DOMException('Fixture quota','QuotaExceededError');
        return store.call(this,key,value);
      };
    });
    await page.goto(server.origin);await waitFor(page,channel);await page.locator(channel).click();
    await page.locator(`#posts [data-post-id="${root.id}"] .reply-count`).click();
    const form=page.locator('#thread-form'), check=form.getByRole('checkbox'), draft=form.locator('textarea');
    const send=form.locator('button[type=submit]'), text='Share this draft after connectivity returns';
    await check.check();await draft.fill(text);
    await page.evaluate(()=>{window.fixtureOnline=false;window.fixtureDenyStore=true;dispatchEvent(new Event('offline'));});
    await waitFor(page,'#app[data-phase=offline]');await send.click();
    expect(await draft.inputValue()).toBe(text);
    await page.evaluate(()=>{window.fixtureDenyStore=false;});await send.click();
    expect(await draft.inputValue()).toBe('');
    const [saved]=await page.evaluate(()=>JSON.parse(localStorage.getItem('atm-chat-outbox-1')));
    expect(saved.slice(2,5)).toEqual([text,root.id,true]);
    expect(await check.isChecked()).toBe(false);
  } finally {await context.close();}
},45_000);

test('restores the checked target with the same draft after leaving a thread',async()=>{
  const context=await browser.newContext({viewport:{width:390,height:844}});
  try {
    const page=await context.newPage(), root=server.fake.inject('demo','A thread to revisit');
    server.fake.inject('demo','First reply',{thread:root.id});
    await page.addInitScript(()=>Object.defineProperty(navigator,'onLine',{value:true}));
    await page.goto(server.origin);await waitFor(page,channel);await page.locator(channel).click();
    const open=page.locator(`#posts [data-post-id="${root.id}"] .reply-count`);
    await open.click();
    const form=page.locator('#thread-form'), draft=form.locator('textarea'), check=form.getByRole('checkbox');
    await draft.fill('A saved thread draft');await check.check();
    await page.locator('#thread-back').click();await waitFor(page,'#app[data-pane=channel]');
    await open.click();await waitFor(page,'#app[data-pane=thread]');
    expect(await draft.inputValue()).toBe('A saved thread draft');
    expect(await check.isChecked()).toBe(true);
    await check.uncheck();
    await page.locator('#thread-back').click();await waitFor(page,'#app[data-pane=channel]');await open.click();
    expect(await check.isChecked()).toBe(false);
  } finally {await context.close();}
},45_000);

test.each(['history','receipt'])('reconciles %s first without hiding the confirmed row behind its successor',async order=>{
  const context=await browser.newContext({viewport:{width:840,height:760}});
  const entered=Promise.withResolvers(), releasePost=Promise.withResolvers(), receipt=Promise.withResolvers(), releaseReceipt=Promise.withResolvers();
  const follow=Promise.withResolvers();
  const text=`One visible echo with ${order} first`;
  try {
    const page=await context.newPage();
    await page.addInitScript(()=>Object.defineProperty(navigator,'onLine',{value:true}));
    let request;
    await page.routeWebSocket('**/ws',client=>{
      const upstream=client.connectToServer();
      client.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.method==='channel.post'&&f.params.body==='Second post still in flight') {follow.resolve();return;}
        if(f.method==='channel.post'&&f.params.body===text) {
          request=f;entered.resolve();void releasePost.promise.then(()=>upstream.send(raw));return;
        }
        upstream.send(raw);
      });
      upstream.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(request&&f.kind==='channel.posted'&&f.body?.client_key===request.params.client_key) {
          // History carries no client key. Also exercise a missing watch frame.
          if(order==='history') client.send(JSON.stringify({...f,body:{channel:f.body.channel}}));
          return;
        }
        if(request&&f.id===request.id) {
          receipt.resolve(f.result);void releaseReceipt.promise.then(()=>client.send(raw));return;
        }
        client.send(raw);
      });
    });
    await page.goto(server.origin);await waitFor(page,channel);await page.locator(channel).click();
    const textInput=page.locator('#channel-form textarea');
    await textInput.fill(text);await page.locator('#channel-form button[type=submit]').click();await entered.promise;
    const row=page.locator('#posts .msg').filter({hasText:text}), anchor=await row.getAttribute('data-scroll-id');
    expect(await row.getAttribute('data-post-id')).toBeNull();
    await textInput.fill('Second post still in flight');await page.locator('#channel-form button[type=submit]').click();
    releasePost.resolve();const {post_id}=await receipt.promise;
    if(order==='history') {
      const peer=server.fake.inject('demo','Peer receipt fence');
      await waitFor(page,`#posts [data-post-id="${peer.id}"]`);
      // Without exact proof, show history and keep an unresolved local status, not two copies of the body.
      expect(await row.getAttribute('data-post-id')).toBe(post_id);
      expect(await row.getAttribute('data-scroll-id')).not.toBe(anchor);
      expect(await page.locator('#posts .outbox').filter({hasText:'Sending…'}).count()).toBe(2);
    }
    expect(await row.count()).toBe(1);
    releaseReceipt.resolve();await follow.promise;
    const fence=server.fake.inject('demo','History after receipt');
    await waitFor(page,`#posts [data-post-id="${fence.id}"]`);
    expect(await row.count()).toBe(1);
    expect(await row.getAttribute('data-post-id')).toBe(post_id);
    expect(await row.getAttribute('data-scroll-id')).toBe(anchor);
    expect(await textInput.evaluate(i=>i===document.activeElement)).toBe(true);
  } finally {await context.close();}
},45_000);

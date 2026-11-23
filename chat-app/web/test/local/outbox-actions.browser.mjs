import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, launch, openChannel, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const sheet=page=>page.locator('#message-actions[open]');
const trigger=row=>row.locator('.message-trigger');
const names=page=>sheet(page).getByRole('button').allTextContents();
const refused=page=>page.evaluate(()=>new Promise(done=>{
  const obs=new MutationObserver(check);
  function check(){if(document.querySelector('#posts .outbox .post-status')?.textContent==='Not sent'){obs.disconnect();done();}}
  obs.observe(document,{childList:true,subtree:true});check();
}));
const submit=async(page,body)=>{
  await page.locator('#channel-form textarea').fill(body);
  await page.locator('#channel-form button[type=submit]').click();
};

function holdPosts(page,stop) {
  return page.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer();
    upstream.onMessage(raw=>client.send(raw));
    client.onMessage(raw=>{
      const f=JSON.parse(String(raw));
      if(f.method==='channel.post') stop(f,()=>upstream.send(raw),()=>client.send(JSON.stringify({
        type:'response',id:f.id,error:{code:'refused',message:'Fixture refusal'},
      })));
      else upstream.send(raw);
    });
  });
}

test('keeps the owner avatar and body aligned while sending and after refusal',()=>withChat(browser,async(page,server)=>{
  const held=Promise.withResolvers(), body='A local message on the avatar rail';
  const owner=server.fake.inject('demo','Already sent by you',{author:'owner'});
  await holdPosts(page,(_f,_send,refuse)=>held.resolve(refuse));
  await openChannel(page,server);const canonical=await waitForPost(page,owner.id);
  await submit(page,body);const refuse=await held.promise;
  const row=page.locator('#posts .outbox').filter({hasText:body}), badge=row.locator('.avatar');
  const check=async status=>{
    expect(await row.locator('.post-status').textContent()).toBe(status);
    expect(await badge.allTextContents()).toEqual(['Y']);
    expect(await badge.isVisible()).toBe(true);
    expect(await badge.getAttribute('aria-hidden')).toBe('true');
    expect(await row.locator('time').count()).toBe(0);
    expect((await row.locator('.body').boundingBox()).x).toBeCloseTo((await canonical.locator('.body').boundingBox()).x,1);
  };
  await check('Sending…');
  refuse();await refused(page);await check('Not sent');
},{viewport:{width:360,height:787},deviceScaleFactor:3,hasTouch:true,isMobile:true}));

test('a sending local sheet is copy/close only and restores focus to the canonical row',()=>withChat(browser,async(page,server)=>{
  const held=Promise.withResolvers(), body='A sending row with its sheet still open';
  await holdPosts(page,(_f,release)=>held.resolve(release));
  await openChannel(page,server);await submit(page,body);const release=await held.promise;
  const row=page.locator('#posts .outbox').filter({hasText:body});
  const anchor=await row.getAttribute('data-scroll-id');
  expect(await row.getAttribute('data-post-id')).toBeNull();
  expect(await row.locator('[data-end]').count()).toBe(0);
  expect(await trigger(row).count()).toBe(1);
  await trigger(row).press('Enter');expect(await names(page)).toEqual(['Copy text','Close']);
  release();
  const confirmed=`#posts [data-post-id][data-scroll-id="${anchor}"]`;
  await dom(page,confirmed);
  expect(await sheet(page).count()).toBe(0);
  expect(await page.locator(`${confirmed} .message-trigger`).evaluate(n=>n===document.activeElement)).toBe(true);
}));

test('failed local rows offer Retry/Delete by keyboard and touch and recover focus after Delete',()=>withChat(browser,async(page,server)=>{
  const time=Date.UTC(2026,0,1);
  await page.clock.install({time});await page.clock.pauseAt(time+60_000);
  await holdPosts(page,(_f,_send,refuse)=>refuse());
  await openChannel(page,server);await submit(page,'Definitely refused');
  await refused(page);
  const row=page.locator('#posts .outbox').filter({hasText:'Definitely refused'});
  expect(await row.locator('.post-status').textContent()).toBe('Not sent');
  expect(await trigger(row).count()).toBe(1);
  await trigger(row).press('Enter');expect(await names(page)).toEqual(['Retry','Delete','Copy text','Close']);
  await page.keyboard.press('Escape');
  const at=row.locator('.body');await at.scrollIntoViewIfNeeded();
  const rect=await at.boundingBox(), cdp=await page.context().newCDPSession(page);
  await cdp.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x:rect.x+5,y:rect.y+5,id:1}]});
  await page.clock.runFor(500);
  await cdp.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
  expect(await names(page)).toEqual(['Retry','Delete','Copy text','Close']);
  await page.clock.resume();
  await sheet(page).getByRole('button',{name:'Delete',exact:true}).tap();
  expect(await row.count()).toBe(0);expect(await sheet(page).count()).toBe(0);
  expect(await page.locator('#channel-title').evaluate(n=>n===document.activeElement)).toBe(true);
},{hasTouch:true}));

test('an inaccessible refused target has a local copy and delete path at Home',()=>withChat(browser,async(page,server)=>{
  const held=Promise.withResolvers();let absent=false;
  await page.routeWebSocket('**/ws',client=>{
    const upstream=client.connectToServer(), lists=new Set();
    upstream.onMessage(raw=>{
      const f=JSON.parse(String(raw));
      if(absent&&lists.has(f.id)&&f.result?.channels) f.result.channels=f.result.channels.filter(c=>c.id!=='demo');
      client.send(JSON.stringify(f));
    });
    client.onMessage(raw=>{
      const f=JSON.parse(String(raw));
      if(f.method==='channel.list') lists.add(f.id);
      if(f.method==='channel.post') held.resolve(()=>client.send(JSON.stringify({type:'response',id:f.id,
        error:{code:'unknown_channel',message:'Fixture channel removed'},
      })));
      else upstream.send(raw);
    });
  });
  const body='Text retained after the destination disappeared';
  await openChannel(page,server);await submit(page,body);const refuse=await held.promise;
  absent=true;refuse();await refused(page);
  const fence=server.fake.inject('demo/chief+owner','New list without the destination');
  await waitForPost(page,fence.id);
  expect(await page.locator('#channels [data-channel="demo"]').count()).toBe(0);
  await page.goBack();await dom(page,'#app[data-pane=home]');
  const recovery=page.getByText('1 message not sent',{exact:true});
  expect(await recovery.count()).toBe(1);
  await recovery.press('Enter');
  const row=page.locator('#needs .outbox').filter({hasText:body});
  expect(await row.getByText('#demo',{exact:true}).count()).toBe(1);
  await trigger(row).press('Enter');expect(await names(page)).toEqual(['Delete','Copy text','Close']);
  await sheet(page).getByRole('button',{name:'Delete',exact:true}).press('Enter');
  expect(await recovery.isVisible()).toBe(false);
  expect(await page.evaluate(()=>localStorage.getItem('atm-chat-outbox-1'))).toBeNull();
  expect(await page.locator('#needs-title').evaluate(n=>n===document.activeElement)).toBe(true);
}));

test('Home lists even reachable parked rows and names a same-body reply separately',()=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Reachable thread');
  server.fake.inject('demo','Prior reply',{thread:root.id});
  await page.addInitScript(id=>localStorage.setItem('atm-chat-outbox-1',JSON.stringify([
    ['demo','first','Same queued text',null,null,null,null,true],
    ['demo','second','Same queued text',id,null,null,null,true],
  ])),root.id);
  await page.goto(server.origin);await dom(page,`#app[data-phase=live] #posts [data-post-id="${root.id}"]`);
  const recovery=page.locator('#needs details').filter({has:page.getByText(/messages? not sent$/)});
  expect(await recovery.isVisible()).toBe(true);
  expect(await recovery.locator('summary').textContent()).toBe('2 messages not sent');
  await recovery.locator('summary').press('Enter');
  const rows=recovery.locator('.outbox');
  expect(await rows.locator('.body').allTextContents()).toEqual(['Same queued text','Same queued text']);
  expect(await rows.getByText('#demo',{exact:true}).count()).toBe(1);
  expect(await rows.getByText('Reply in #demo',{exact:true}).count()).toBe(1);
  await trigger(rows.filter({hasText:'Reply in #demo'})).press('Enter');
  expect(await names(page)).toEqual(['Delete','Copy text','Close']);
  await sheet(page).getByRole('button',{name:'Delete',exact:true}).press('Enter');
  expect(await rows.count()).toBe(1);
  expect(await page.evaluate(()=>JSON.parse(localStorage.getItem('atm-chat-outbox-1')).map(row=>row[1]))).toEqual(['first']);
}));

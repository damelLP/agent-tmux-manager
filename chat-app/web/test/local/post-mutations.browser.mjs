import { join } from 'node:path';
import { afterAll, beforeAll, expect, it } from 'vitest';
import { dom, frame, launch, openChannel, openThread, postSelector, waitForPost, withChat } from './message-rig.mjs';
import { touchDrag } from './touch.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>browser?.close());
const button=(row,name)=>row.getByRole('button',{name,exact:true});
const trigger=row=>button(row,'Message actions');
const actions=page=>page.getByRole('dialog',{name:'Message actions'});
const mobile={viewport:{width:360,height:787},hasTouch:true,isMobile:true};
const focus=locator=>locator.evaluate(e=>document.activeElement===e);
async function openActions(page,row,key='Enter') {
  expect(await trigger(row).count()).toBe(1);
  await trigger(row).press(key);
  expect(await actions(page).isVisible()).toBe(true);
  return actions(page);
}
async function pauseClock(page) {
  await frame(page);
  const time=Date.now();
  await page.clock.install({time});
  await page.clock.pauseAt(time+60_000);
}
async function fingerAt(page,target) {
  await target.scrollIntoViewIfNeeded();
  const b=await target.boundingBox(), x=b.x+b.width/2, y=b.y+b.height/2;
  const cdp=await page.context().newCDPSession(page);
  const send=(type,touchPoints)=>cdp.send('Input.dispatchTouchEvent',{type,touchPoints});
  const finger={x,y,id:1};
  return {start:()=>send('touchStart',[finger]), end:()=>send('touchEnd',[]),
    cancel:()=>send('touchCancel',[]), second:()=>send('touchStart',[finger,{x:x+22,y:y+10,id:2}]),
    drag:options=>touchDrag(cdp,x,y,0,100,options)};
}
const shot=async(page,name)=>{
  if(process.env.ATM_CHAT_TEST_SHOTS) await page.screenshot({path:join(process.env.ATM_CHAT_TEST_SHOTS,`${name}.png`)});
};
const calls=(server,method)=>server.frames.filter(f=>f.method===method).map(f=>f.params);
const text=(page,selector,copy)=>page.evaluate(([selector,copy])=>new Promise(done=>{
  const observer=new MutationObserver(check);
  function check(){if(document.querySelector(selector)?.textContent===copy){observer.disconnect();done();}}
  observer.observe(document,{subtree:true,childList:true,characterData:true});check();
}),[selector,copy]);

it('edits inline without losing a draft on an unrelated frame, and cancels without a write',()=>withChat(browser,async(page,server)=>{
  const own=server.fake.inject('demo','Original owner post',{author:'owner'});
  const peer=server.fake.inject('demo','A seat wrote this');
  const legacy=server.fake.inject('demo','Legacy owner post',{author:'owner',legacy:true});
  await openChannel(page,server);
  const row=await waitForPost(page,own.id);
  expect(await button(row,'Edit').count()).toBe(0);
  expect(await button(row,'Remove').count()).toBe(0);
  for(const entry of [peer,legacy]) {
    const menu=await openActions(page,await waitForPost(page,entry.id));
    expect(await button(menu,'Edit').count()).toBe(0);
    expect(await button(menu,'Remove').count()).toBe(0);
    expect(await button(menu,'Thumbs up').count()).toBe(entry===peer?1:0);
    await page.keyboard.press('Escape');
  }
  await button(await openActions(page,row),'Edit').click();
  const editor=row.getByRole('textbox',{name:'Edit message'});
  expect(await editor.inputValue()).toBe('Original owner post');
  await editor.fill('Discard this');await button(row,'Cancel').click();
  expect(calls(server,'channel.edit')).toEqual([]);
  expect(await row.locator('.body').textContent()).toBe('Original owner post');
  await button(await openActions(page,row),'Edit').click();await editor.fill('Revised <draft>');
  await editor.evaluate(e=>e.setSelectionRange(3,7));
  const arriving=server.fake.inject('demo','An unrelated frame');
  await dom(page,postSelector(arriving.id));
  expect(await editor.inputValue()).toBe('Revised <draft>');
  expect(await editor.evaluate(e=>[document.activeElement===e,e.selectionStart,e.selectionEnd])).toEqual([true,3,7]);
  await shot(page,'edit-inline');
  await button(row,'Save').click();
  await text(page,`${postSelector(own.id)} .body`,'Revised <draft>');
  expect(await row.getByText('edited',{exact:true}).count()).toBe(1);
  expect(calls(server,'channel.edit')).toEqual([{post:own.id,body:'Revised <draft>'}]);
}));

it('keeps the same typed edit open through an offline save and never replays it on reconnect',()=>withChat(browser,async(page,server)=>{
  const own=server.fake.inject('demo','Original owner post',{author:'owner'});
  await openChannel(page,server);
  const row=await waitForPost(page,own.id), edit=row.getByRole('textbox',{name:'Edit message'}), save=button(row,'Save');
  await button(await openActions(page,row),'Edit').click();
  await edit.fill('Keep this unsent edit');
  // Deliberately expose the new network status before the offline event is delivered.
  await page.evaluate(()=>Object.defineProperty(navigator,'onLine',{value:false,configurable:true}));
  await save.click();
  expect(await edit.count()).toBe(1);
  expect(calls(server,'channel.edit')).toEqual([]);
  await page.evaluate(()=>dispatchEvent(new Event('offline')));
  await dom(page,'#app[data-phase=offline]');
  expect(await edit.inputValue()).toBe('Keep this unsent edit');
  expect(await save.isDisabled()).toBe(true);
  await page.evaluate(()=>{delete navigator.onLine;dispatchEvent(new Event('online'));});
  await dom(page,'#app[data-phase=live]');
  expect(await edit.inputValue()).toBe('Keep this unsent edit');
  expect(await save.isEnabled()).toBe(true);
  expect(calls(server,'channel.edit')).toEqual([]);
  await save.click();
  expect(await edit.count()).toBe(0);
  await text(page,`${postSelector(own.id)} .body`,'Keep this unsent edit');
  expect(calls(server,'channel.edit')).toEqual([{post:own.id,body:'Keep this unsent edit'}]);
}));

it('confirms Remove, refreshes the displayed root and quote, and still lets the owner reply',()=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Owner root to remove',{author:'owner',attachments:['note.txt']});
  const shared=server.fake.inject('demo','Shared reply stays',{thread:root.id,also_in_chat:true});
  await openChannel(page,server);
  const row=await waitForPost(page,root.id);
  await button(await openActions(page,row),'Remove').click();await button(page.getByRole('dialog'),'Cancel').click();
  expect(await focus(trigger(row))).toBe(true);
  expect(calls(server,'channel.remove')).toEqual([]);
  await button(await openActions(page,row),'Remove').click();await button(page.getByRole('dialog'),'Remove message').click();
  await text(page,`${postSelector(root.id)} .body`,'removed');
  const quote=(await waitForPost(page,shared.id)).locator('.quote');
  await text(page,`${postSelector(shared.id)} .quote`,'Replied in a thread · removed');
  expect(await quote.count()).toBe(1);
  expect(await row.locator('.files').count()).toBe(0);
  const menu=await openActions(page,row);
  for(const label of ['Edit','Remove','Thumbs up']) expect(await button(menu,label).count()).toBe(0);
  expect(await button(menu,'Reply in thread').count()).toBe(1);
  await page.keyboard.press('Escape');
  expect(calls(server,'channel.remove')).toEqual([{post:root.id}]);
  await shot(page,'removed-row');
  await openThread(page,root.id);
  await text(page,`${postSelector(root.id,'replies')} .body`,'removed');
  expect(await page.locator('#thread-form textarea').isVisible()).toBe(true);
  expect(await (await waitForPost(page,shared.id,'replies')).locator('.body').textContent()).toBe('Shared reply stays');
}));

it('opens one sheet at 500 ms on a 360×787 phone and does not activate it on release',()=>withChat(browser,async(page,server)=>{
  const item=server.fake.inject('demo','A teammate’s message with a reaction');
  const fresh=server.fake.inject('demo','Hold this owner message',{author:'owner'});
  server.fake.reactAs(item.id,'👍','demo/worker');
  await openChannel(page,server);
  const row=await waitForPost(page,item.id), own=await waitForPost(page,fresh.id);
  const chips=post=>post.locator('button[aria-pressed]').allTextContents();
  expect(await chips(row)).toEqual(['👍 1']);
  expect(await chips(own)).toEqual([]);
  const target=own.locator('.body'), menu=actions(page);
  await target.tap();
  expect(await menu.count()).toBe(0);
  const finger=await fingerAt(page,target);
  await pauseClock(page);
  await finger.start();
  await page.clock.runFor(499);
  expect(await menu.count()).toBe(0);
  await page.clock.runFor(1);
  const opened=await menu.count();
  await finger.end();
  await page.clock.resume();
  expect(opened).toBe(1);
  expect(await menu.count()).toBe(1);
  expect(await page.evaluate(()=>String(document.getSelection()))).toBe('');
  expect(calls(server,'channel.react')).toEqual([]);
  const choices=menu.getByRole('group',{name:'Choose reaction'});
  expect(await choices.getByRole('button').allTextContents()).toEqual(['👍','✅','👀','❌']);
  const boxes=await Promise.all(['Thumbs up','Check mark','Eyes','Cross mark'].map(name=>button(choices,name).boundingBox()));
  expect(boxes.every(b=>b.width>=44&&b.height>=44&&b.y===boxes[0].y)).toBe(true);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBe(360);
  await shot(page,'message-actions-360');
  await button(choices,'Check mark').tap();
  expect(await menu.count()).toBe(0);
  await dom(page,`${postSelector(fresh.id)} button[aria-label="Check mark, 1"][aria-pressed="true"]:not([disabled])`);
  const chip=count=>button(row,`Thumbs up, ${count}`);
  expect(await chip(1).getAttribute('aria-pressed')).toBe('false');
  await chip(1).tap();await dom(page,`${postSelector(item.id)} button[aria-label="Thumbs up, 2"][aria-pressed="true"]:not([disabled])`);
  expect(await chips(row)).toEqual(['👍 2']);
  await chip(2).tap();await dom(page,`${postSelector(item.id)} button[aria-label="Thumbs up, 1"][aria-pressed="false"]:not([disabled])`);
  expect(await chips(row)).toEqual(['👍 1']);
  await button(own,'Check mark, 1').tap();
  await dom(page,`${postSelector(fresh.id)}:not(:has(button[aria-pressed]))`);
  expect(calls(server,'channel.react')).toEqual([
    {post:fresh.id,emoji:'✅'},{post:item.id,emoji:'👍'},{post:item.id,emoji:'👍'},{post:fresh.id,emoji:'✅'},
  ]);
},mobile));

it('cancels on drag, second finger, cancel, row replacement and view change',()=>withChat(browser,async(page,server)=>{
  const item=server.fake.inject('demo','Stationary hold is required');
  await openChannel(page,server);
  const row=await waitForPost(page,item.id), target=row.locator('.body');
  let finger=await fingerAt(page,target);
  await pauseClock(page);
  const tick=()=>page.clock.runFor(500);
  const closed=async()=>expect(await actions(page).count()).toBe(0);
  await finger.start();await finger.end();await tick();await closed();
  await finger.drag({beforeEnd:async()=>{await tick();await closed();}});
  finger=await fingerAt(page,target); // The real scroll changed which row is under the old coordinates.
  await finger.start();await finger.second();await tick();await closed();await finger.end();
  await finger.start();await finger.cancel();await tick();await closed();
  await finger.start();
  server.fake.reactAs(item.id,'👀','demo/worker');
  await dom(page,`${postSelector(item.id)} button[aria-label="Eyes, 1"]`);
  await tick();await closed();await finger.end();
  await finger.start();await page.goBack();await dom(page,'#app[data-pane="home"]');
  await tick();await closed();await finger.end();
  expect(calls(server,'channel.react')).toEqual([]);
},mobile));

it('accepts a native hold before the app timer but yields if native selection started',()=>withChat(browser,async(page,server)=>{
  const item=server.fake.inject('demo','A browser may signal the hold first');
  await openChannel(page,server);
  const row=await waitForPost(page,item.id), target=row.locator('.body'), menu=actions(page);
  await target.evaluate(e=>e.onpointerdown=p=>{e.dataset.touchId=String(p.pointerId);});
  const finger=await fingerAt(page,target);
  await pauseClock(page);
  const nativeHold=()=>target.evaluate(e=>e.dispatchEvent(new PointerEvent('contextmenu',{
    bubbles:true,cancelable:true,button:2,pointerType:'touch',pointerId:Number(e.dataset.touchId),
  })));
  await finger.start();await page.clock.runFor(499);
  expect(await nativeHold()).toBe(false); // The browser's own hold should not also show its menu.
  await page.clock.runFor(1);await finger.end();
  expect(await menu.count()).toBe(1);
  await page.keyboard.press('Escape');
  await finger.start();
  expect(await target.evaluate(e=>e.firstChild.dispatchEvent(new Event('selectstart',{bubbles:true,cancelable:true})))).toBe(true);
  expect(await nativeHold()).toBe(true);
  await page.clock.runFor(500);await finger.end();
  expect(await menu.count()).toBe(0);
},mobile));

it('leaves link holds and existing selection native, while eligible right-click opens and backdrop dismisses',()=>withChat(browser,async(page,server)=>{
  const item=server.fake.inject('demo','Selectable text and [a link](https://example.test/)');
  await openChannel(page,server);
  const row=await waitForPost(page,item.id), target=row.locator('.body'), link=row.getByRole('link',{name:'a link'});
  await link.evaluate(a=>{a.onclick=e=>{e.preventDefault();a.dataset.tapped='yes';};});
  const finger=await fingerAt(page,link);
  await pauseClock(page);
  await finger.start();await page.clock.runFor(500);await finger.end();
  expect(await actions(page).count()).toBe(0);
  await page.clock.resume();
  await link.evaluate(a=>{delete a.dataset.tapped;});
  await link.tap();expect(await link.getAttribute('data-tapped')).toBe('yes');
  await target.evaluate(e=>{
    const selection=document.getSelection(), range=document.createRange();
    range.selectNodeContents(e);selection.removeAllRanges();selection.addRange(range);
  });
  await target.dispatchEvent('contextmenu',{button:2,pointerType:'mouse',bubbles:true,cancelable:true});
  expect(await actions(page).count()).toBe(0);
  expect(await page.evaluate(()=>String(document.getSelection()))).toBe('Selectable text and a link');
  await page.evaluate(()=>document.getSelection().removeAllRanges());
  await link.click({button:'right'});expect(await actions(page).count()).toBe(0);
  await target.click({button:'right',position:{x:8,y:8}});
  expect(await actions(page).count()).toBe(1);
  await page.touchscreen.tap(8,8);
  expect(await actions(page).count()).toBe(0);
},mobile));

it('copies current body including literal markup, and announces clipboard denial',()=>withChat(browser,async(page,server)=>{
  const body='Copy **the body**, not the UI', item=server.fake.inject('demo',body);
  await page.context().grantPermissions(['clipboard-read','clipboard-write'],{origin:server.origin});
  await openChannel(page,server);
  const row=await waitForPost(page,item.id), menu=await openActions(page,row);
  await button(menu,'Copy text').click();
  await dom(page,'dialog[aria-label="Message actions"]:not([open])');
  expect(await page.evaluate(()=>navigator.clipboard.readText())).toBe(body);
  await page.evaluate(()=>Object.defineProperty(navigator.clipboard,'writeText',{value:()=>Promise.reject(new DOMException('Denied','NotAllowedError'))}));
  await button(await openActions(page,row),'Copy text').click();
  await text(page,'dialog[open] [role=status]',"Couldn't copy text.");
  expect(await actions(page).isVisible()).toBe(true);
  expect(await page.evaluate(()=>navigator.clipboard.readText())).toBe(body);
}));

it('exposes only Copy text for a read-only conversation',()=>withChat(browser,async(page,server)=>{
  await openChannel(page,server);
  await page.locator('#home-back').click();
  await page.getByText('Behind the scenes · #demo',{exact:true}).click();
  await page.locator('[data-channel="demo/lead+demo/worker"]').click();
  const row=page.locator('#posts article',{hasText:'The read-only example.'});
  await dom(page,'#posts article');
  const menu=await openActions(page,row);
  expect(await menu.getByRole('button').allTextContents()).toEqual(['Copy text','Close']);
}));

it('contains keyboard focus in the sheet and returns to the invoking copy of a shared reply',()=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Root for keyboard reactions');
  const reply=server.fake.inject('demo','One shared copy',{thread:root.id,also_in_chat:true});
  await openChannel(page,server);
  await openThread(page,root.id);
  const row=await waitForPost(page,reply.id,'replies'), main=await waitForPost(page,reply.id), from=trigger(row);
  const menu=await openActions(page,row,'Shift+F10'), first=button(menu,'Thumbs up');
  expect(await focus(first)).toBe(true);
  expect(await trigger(main).getAttribute('aria-expanded')).toBe('false');
  await page.keyboard.press('Shift+Tab');
  expect(await focus(button(menu,'Close'))).toBe(true);
  await page.keyboard.press('Tab');
  expect(await focus(first)).toBe(true);
  await page.keyboard.press('Escape');
  expect(await focus(from)).toBe(true);
  expect(await menu.count()).toBe(0);
  expect(calls(server,'channel.react')).toEqual([]);
  await page.keyboard.press('Space');
  expect(await focus(first)).toBe(true);
  await page.keyboard.press('Tab');
  expect(await focus(button(menu,'Check mark'))).toBe(true);
  await page.keyboard.press('Enter');
  await dom(page,`${postSelector(reply.id,'replies')} button[aria-label="Check mark, 1"][aria-pressed="true"]:not([disabled])`);
  await dom(page,`${postSelector(reply.id)} button[aria-label="Check mark, 1"][aria-pressed="true"]:not([disabled])`);
  expect(await from.getAttribute('aria-expanded')).toBe('false');
  expect(await focus(from)).toBe(true);
  await button(row,'Check mark, 1').focus();
  server.fake.reactAs(reply.id,'✅','demo/worker');
  await dom(page,`${postSelector(reply.id,'replies')} button[aria-label="Check mark, 2"]`);
  expect(await focus(button(row,'Check mark, 2'))).toBe(true);
  expect(calls(server,'channel.react')).toEqual([{post:reply.id,emoji:'✅'}]);
},{viewport:{width:1897,height:1100}}));

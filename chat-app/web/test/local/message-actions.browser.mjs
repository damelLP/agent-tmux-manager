import { afterAll, beforeAll, expect, it } from 'vitest';
import { dom, frame, launch, openChannel, openThread, postSelector, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>browser?.close());
const button=(scope,name)=>scope.getByRole('button',{name,exact:true});
const trigger=row=>button(row,'Message actions');
const actions=page=>page.getByRole('dialog',{name:'Message actions'});
const focused=locator=>locator.evaluate(el=>el===document.activeElement);
const narrow={width:360,height:787};
const wide={width:1450,height:900};

it('a held Enter never activates the newly focused message action or reopens the sheet',()=>withChat(browser,async(page,server)=>{
  const post=server.fake.inject('demo','Open actions with the keyboard');
  await openChannel(page,server);
  const row=await waitForPost(page,post.id), from=trigger(row), menu=actions(page);
  await from.focus();
  await page.keyboard.down('Enter');
  expect(await focused(button(menu,'Thumbs up'))).toBe(true);
  await page.keyboard.down('Enter'); // A second down without an up is a trusted key repeat.
  await page.keyboard.up('Enter');
  expect(await menu.isVisible(),'the opening key must not choose a reaction').toBe(true);
  expect(server.frames.filter(f=>f.method==='channel.react')).toEqual([]);

  await page.keyboard.down('Enter'); // A new press really chooses the focused reaction.
  // Wait for the keyed row to settle before querying its replacement trigger, but leave Enter held.
  await dom(page,`${postSelector(post.id)} button[aria-label="Thumbs up, 1"][aria-pressed="true"]:not([disabled])`);
  expect(await focused(from)).toBe(true);
  await page.keyboard.down('Enter');
  await page.keyboard.up('Enter');
  expect(await menu.count(),'the choosing key must not reopen actions').toBe(0);
  expect(server.frames.filter(f=>f.method==='channel.react').map(f=>f.params)).toEqual([{post:post.id,emoji:'👍'}]);
}));

it('Copy text accepts one pending write and permits retry after denial',()=>withChat(browser,async(page,server)=>{
  const body='Clipboard attempt in progress', post=server.fake.inject('demo',body);
  await openChannel(page,server);
  // Control only completion of the platform write so successive keys overlap deterministically.
  await page.evaluate(()=>{
    const writes=[];
    window.copyProbe={writes,deny:()=>writes.at(-1).reject(new DOMException('Denied','NotAllowedError')),
      finish:()=>writes.at(-1).resolve()};
    Object.defineProperty(navigator.clipboard,'writeText',{value:body=>new Promise((resolve,reject)=>writes.push({body,resolve,reject}))});
  });
  const row=await waitForPost(page,post.id), menu=actions(page);
  await trigger(row).press('Enter');
  const copy=button(menu,'Copy text');
  const bodies=()=>page.evaluate(()=>window.copyProbe.writes.map(write=>write.body));
  await copy.press('Enter');await copy.press('Enter');
  expect(await bodies()).toEqual([body]);
  await page.evaluate(()=>window.copyProbe.deny());
  await dom(page,'#message-actions [role="status"]:not(:empty)');
  expect(await menu.isVisible()).toBe(true);
  expect(await focused(copy)).toBe(true);
  await copy.press('Enter');
  expect(await bodies()).toEqual([body,body]);
  await page.evaluate(()=>window.copyProbe.finish());
  await dom(page,'#message-actions:not([open])');
  expect(await focused(trigger(row))).toBe(true);
}));

it.each(['Cancel','Remove message'])('%s returns focus to the current thread copy of the invoking post',label=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Root for Remove focus');
  const reply=server.fake.inject('demo','Shared owner reply',{author:'owner',thread:root.id,also_in_chat:true});
  await openChannel(page,server);
  await openThread(page,root.id);
  const row=await waitForPost(page,reply.id,'replies'), from=trigger(row);
  await from.press('Enter');
  await button(actions(page),'Remove').click();
  const confirm=page.locator('#decision-confirm');
  if(label==='Cancel') {
    server.fake.reactAs(reply.id,'👀','demo/worker');
    await dom(page,`${postSelector(reply.id,'replies')} button[aria-label="Eyes, 1"]`);
  }
  await button(confirm,label).click();
  await dom(page,'#decision-confirm:not([open])');
  const current=postSelector(reply.id,'replies');
  if(label==='Remove message') await dom(page,`${current} .body.post-status`);
  // Check the live focus and its copy atomically while receipt frames can still replace rows.
  expect(await page.evaluate(selector=>document.activeElement?.matches(selector),`${current} > .message-trigger`)).toBe(true);
  expect(server.frames.filter(f=>f.method==='channel.remove').map(f=>f.params)).toEqual(label==='Cancel'?[]:[{post:reply.id}]);
},{viewport:narrow}));

it('dismisses the sheet to the visible thread heading when folding hides its channel invoker',()=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Thread with an adjacent channel copy');
  server.fake.inject('demo','Reply',{thread:root.id});
  await openChannel(page,server);
  await openThread(page,root.id);
  const row=await waitForPost(page,root.id);
  await trigger(row).press('Enter');
  expect(await actions(page).isVisible()).toBe(true);
  await page.setViewportSize(narrow);
  await dom(page,'#thread-back[aria-label="Back"]');
  await frame(page); // Run layout and deliver resize notifications before checking focus.
  expect(await row.isVisible()).toBe(false);
  expect(await actions(page).count()).toBe(0);
  expect(await focused(page.locator('#thread-title'))).toBe(true);
},{viewport:wide}));

it('paints the message action trigger only on keyboard focus, not at rest or on hover',()=>withChat(browser,async(page,server)=>{
  const post=server.fake.inject('demo','Quiet message row',{author:'owner'});
  await openChannel(page,server);
  const row=await waitForPost(page,post.id), control=row.locator('.message-trigger');
  await frame(page);
  // Compare the same row's paint with and without its absolutely positioned trigger.
  const picture=()=>row.screenshot({animations:'disabled'});
  const paintsTrigger=async()=>{
    const normal=await picture();
    await control.evaluate(el=>{el.hidden=true;});
    try {return !normal.equals(await picture());}
    finally {await control.evaluate(el=>{el.hidden=false;});}
  };
  expect(await paintsTrigger(),'at rest').toBe(false);
  await row.locator('.body').hover();
  expect(await paintsTrigger(),'on hover').toBe(false);
  await row.evaluate(el=>el.focus({preventScroll:true}));
  await page.keyboard.press('Tab');
  expect(await focused(trigger(row))).toBe(true);
  expect(await paintsTrigger(),'on keyboard focus').toBe(true);
},{viewport:wide}));

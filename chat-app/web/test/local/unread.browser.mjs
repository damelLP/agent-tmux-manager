import { join } from 'node:path';
import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, frame, launch, openChannel, openThread, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const waitBadge=(page,channel,text)=>page.evaluate(([channel,text])=>new Promise(resolve=>{
  const observer=new MutationObserver(check);
  const badge=`#channels [data-channel="${channel}"] .badge`;
  function check(){if((document.querySelector(badge)?.textContent??'')===text){observer.disconnect();resolve();}}
  observer.observe(document,{subtree:true,childList:true,attributes:true,characterData:true});check();
}),[channel,text]);

async function dividerAt(page,port,id) {
  const line=page.locator('.new-line');
  expect(await line.count()).toBe(1);
  expect(await line.textContent()).toBe('New');
  expect(await line.evaluate(el=>[el.closest('.log')?.id,el.nextElementSibling?.dataset.postId])).toEqual([port,id]);
  return line;
}
async function jumpTo(page,port,id,line) {
  const jump=page.getByRole('button',{name:'Jump to new',exact:true});
  expect(await jump.isVisible()).toBe(true);
  await jump.click(); await frame(page);
  expect(await line.evaluate((el,port)=>Math.abs(el.getBoundingClientRect().top-document.getElementById(port).getBoundingClientRect().top),port)).toBeLessThanOrEqual(2);
  expect(await page.evaluate(()=>document.activeElement.dataset.postId)).toBe(id);
  expect(await jump.count()).toBe(0);
}

test('a phone can jump to a fixed unread line in the main log and thread, including a shared reply',()=>withChat(browser,async(page,server)=>{
  const c='demo', dm='demo/lead+owner', time={posted_at:'2026-01-02T04:40:00.000Z'};
  const note=(body,fields={})=>server.fake.inject(c,body,{...time,...fields});
  const mark=(channel,through)=>server.fake.call('channel.seen',{channel,through});
  const root=note('Thread topic'), read=note('Last read from the same author'); mark(c,read.id);
  const first=note('First unread'), second=note('Unread continuation');
  const fill='A longer unread message that wraps on a narrow handset to leave the older posts off screen. ';
  let last;
  for(let i=0;i<22;i++) last=note(`${fill}${i}`);
  const reply=note('First unread reply',{thread:root.id});
  let tail;
  for(let i=0;i<24;i++) tail=note(`${fill}${i}`,{thread:root.id});
  const dmRoot=server.fake.inject(dm,'Read topic'); mark(dm,dmRoot.id);
  const shared=server.fake.inject(dm,'Shared new reply',{thread:dmRoot.id,also_in_chat:true});

  await openChannel(page,server); await waitForPost(page,last.id); await frame(page);
  // The viewport at the bottom has already advanced the server marker past the divider.
  await waitBadge(page,c,'25');
  const line=await dividerAt(page,'channel-log',first.id);
  expect(await page.locator(`#posts [data-post-id="${first.id}"] .meta`).isVisible()).toBe(true);
  expect(await page.locator(`#posts [data-post-id="${second.id}"] .meta`).isVisible()).toBe(false);
  const shots=process.env.ATM_CHAT_TEST_SHOTS;
  if(shots) await page.screenshot({path:join(shots,'unread-jump.png')});
  await jumpTo(page,'channel-log',first.id,line);
  if(shots) await page.screenshot({path:join(shots,'unread-line.png')});
  await page.locator('#channel-log').evaluate(el=>{el.scrollTop=el.scrollHeight;}); await frame(page);
  expect(await page.getByRole('button',{name:'Jump to new',exact:true}).count()).toBe(0);

  await openThread(page,root.id); await waitForPost(page,tail.id,'replies');
  await waitBadge(page,c,'');
  await jumpTo(page,'thread-log',reply.id,await dividerAt(page,'thread-log',reply.id));
  if(shots) await page.screenshot({path:join(shots,'unread-thread.png')});
  await page.goBack(); await dom(page,'#app[data-pane="channel"]'); await frame(page);
  expect(await page.locator('.new-line').count()).toBe(0);
  await page.goBack(); await dom(page,'#app[data-pane="home"]');
  await page.locator(`#channels [data-channel="${dm}"]`).click();
  await waitForPost(page,shared.id); await waitBadge(page,dm,'');
  await dividerAt(page,'channel-log',shared.id);
  expect(await page.locator(`#posts article[data-post-id="${shared.id}"]`).count()).toBe(1);
}));

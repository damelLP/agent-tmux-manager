import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, frame, launch, openChannel, openThread, postSelector, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const baseTime='2026-01-02T04:40:00.000Z';
const at=seconds=>new Date(Date.parse(baseTime)+seconds*1000).toISOString();
const meta=(page,id,list)=>page.locator(`${postSelector(id,list)} .meta`);
const avatar=(page,id,list)=>page.locator(`${postSelector(id,list)} .avatar`);
const trigger=row=>row.getByRole('button',{name:'Message actions',exact:true});
const replyAction=page=>page.getByRole('dialog',{name:'Message actions'}).getByRole('button',{name:'Reply in thread',exact:true});
const localOffset=(page,id,move)=>page.locator(postSelector(id)).evaluate((row,move)=>{
  const log=document.getElementById('channel-log');
  const offset=()=>row.getBoundingClientRect().top-log.getBoundingClientRect().top;
  if(move!==null) log.scrollTop+=offset()-move;
  return offset();
},move??null);
const bottom=page=>page.locator('#channel-log').evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop);

async function visibleHeaders(page,posts,expected,list) {
  await waitForPost(page,posts.at(-1).id,list);
  expect(await Promise.all(posts.map(p=>meta(page,p.id,list).isVisible()))).toEqual(expected);
  expect(await Promise.all(posts.map(p=>avatar(page,p.id,list).isVisible()))).toEqual(expected);
}

test.each([
  {viewport:{width:360,height:787},deviceScaleFactor:3,hasTouch:true,isMobile:true},
  {viewport:{width:1440,height:900}},
])('aligns an avatar group and fits long content at $viewport.width px',options=>withChat(browser,async(page,server)=>{
  const note=(author,body,fields)=>server.fake.inject('demo',body,{author,posted_at:at(0),...fields});
  const owner=note('owner','A short note from you.');
  // The desktop preview can mark its visible channel read before the click.
  if(options.hasTouch) {
    const read=note('demo/build-5','Last seen from this seat.');
    server.fake.call('channel.seen',{channel:'demo',through:read.id});
  }
  const head=note('demo/build-5','First line of this group.');
  const cont=note('demo/build-5','```\n'+'x'.repeat(160)+'\n```',{attachments:[`/fixture/private/${'guide'.repeat(24)}.txt`]});
  const long=note('demo/'+('longname'.repeat(12)),'A long seat name must wrap.');
  const peer=note('beta/build-5','Same initials; another full seat ID.');
  const again=note('demo/build-5','A later group by the same seat.');
  await openChannel(page,server);
  const row=await waitForPost(page,head.id);await waitForPost(page,again.id);await frame(page);
  const body=await row.locator('.body').boundingBox(), lane=await page.locator('#posts').boundingBox();
  const box=await row.boundingBox();
  expect(body.x-lane.x).toBeCloseTo(options.hasTouch?53:Math.max(0,(lane.width-672)/2)+59,1);
  expect(box.width).toBeCloseTo(Math.min(lane.width,672),1);
  expect((await page.locator(`${postSelector(cont.id)} .body`).boundingBox()).x).toBeCloseTo(body.x,1);
  expect((await row.locator('.author').boundingBox()).x).toBeCloseTo(body.x,1);
  if(options.hasTouch) expect(await page.locator('.new-line').evaluate(n=>n.nextElementSibling?.dataset.postId)).toBe(head.id);
  await visibleHeaders(page,[owner,head,cont,long,peer,again],[true,true,false,true,true,true]);
  const badge=avatar(page,head.id), face=await badge.boundingBox();
  expect([face.width,face.height,body.x-face.x-face.width]).toEqual([32,32,8]);
  expect(await badge.getAttribute('aria-hidden')).toBe('true');
  expect(await avatar(page,owner.id).textContent()).toBe('Y');
  expect(await badge.textContent()).toBe('B5');
  const tint=a=>a.evaluate(n=>getComputedStyle(n).backgroundColor);
  expect(await tint(badge)).toBe(await tint(avatar(page,again.id)));
  expect(await tint(badge)).not.toBe(await tint(avatar(page,peer.id)));
  expect(await page.locator('#channel-log').evaluate(n=>n.scrollWidth-n.clientWidth)).toBe(0);
  expect(await page.locator(postSelector(long.id)).evaluate(n=>n.scrollWidth-n.clientWidth)).toBe(0);
  const compose=await page.locator('#channel-form').boundingBox();
  expect(compose.y+compose.height).toBeCloseTo(options.viewport.height,1);
},options));

test('breaks on local midnight, elapsed gap, author, a decision, and a thread link',()=>withChat(browser,async(page,server)=>{
  const note=(text,seconds,fields)=>server.fake.inject('demo',text,{posted_at:at(seconds),...fields});
  const first=note('First author',0), cont=note('Continuation',30,{session_id:'fixture-session'});
  const other=note('Different author',60,{author:'demo/worker'});
  const later=note('After a gap',361,{author:'demo/worker'});
  const beforeMidnight=note('Last before midnight',1199.999), midnight=note('Next local date',1200);
  const decision=note('Linked choice',1201);
  server.fake.ask('Pick one',false,{channel:'demo',post_id:decision.id});
  const afterDecision=note('After decision',1202), root=note('Root with a link',1203), afterRoot=note('After link',1204);
  const reply=note('First reply',1205,{thread:root.id}), nextReply=note('Second reply',1206,{thread:root.id});
  await openChannel(page,server);
  await dom(page,`${postSelector(decision.id)} .opts`);
  await visibleHeaders(page,[first,cont,other,later,beforeMidnight,midnight,decision,afterDecision,root,afterRoot],
    [true,false,true,true,true,true,true,true,true,true]);
  expect(await page.locator(`${postSelector(decision.id)} .opts button`).count()).toBe(3);
  // The existing thread link must keep its blue link affordance, not the muted pagination color.
  expect(await page.locator(`${postSelector(root.id)} .reply-count`).evaluate(el=>getComputedStyle(el).color)).toBe('rgb(143, 179, 255)');
  const row=page.locator(postSelector(cont.id));
  expect(await row.getAttribute('data-scroll-id')).toBe(cont.id);
  expect(await row.locator('[data-end]').getAttribute('data-end')).toBe(cont.id);
  expect(await row.getAttribute('tabindex')).toBe('-1');
  expect(await row.getAttribute('aria-label')).toMatch(/lead.*fixture-session/);
  expect(await row.getAttribute('aria-label')).toMatch(/11:40:30/);
  await row.locator('.body').tap();
  expect(await replyAction(page).count()).toBe(0);
  await trigger(row).press('Enter');
  expect(await replyAction(page).evaluate(b=>{
    const {width,height}=b.getBoundingClientRect();return height>=44&&width>=44;
  })).toBe(true);
  await replyAction(page).tap();await waitForPost(page,cont.id,'replies');
  await page.goBack();await dom(page,'#app[data-pane="channel"]');
  await openThread(page,root.id);
  await visibleHeaders(page,[root,reply,nextReply],[true,true,false],'replies');
},{hasTouch:true,locale:'en-US',timezoneId:'America/New_York'}));

test('thread links show reply counts and the age of the last reply',()=>withChat(browser,async(page,server)=>{
  const note=(body,seconds,fields)=>server.fake.inject('demo',body,{posted_at:at(seconds),...fields});
  const zero=note('No reply',-600), missing=note('No last reply timestamp',-600,{replies:1});
  const future=note('Clock skew',-600), hours=note('An hour ago',-4500), days=note('Two days ago',-200000);
  note('Future reply',15,{thread:future.id});
  note('Hour-old reply',-3900,{thread:hours.id});
  note('Day-old reply',-179000,{thread:days.id});
  await page.clock.setFixedTime(new Date(baseTime));
  await openChannel(page,server);
  await waitForPost(page,days.id);
  const link=id=>page.locator(`${postSelector(id)} .reply-count`);
  expect(await link(zero.id).count()).toBe(0);
  expect(await Promise.all([missing,future,hours,days].map(post=>link(post.id).textContent()))).toEqual([
    '1 reply','1 reply · last reply just now','1 reply · last reply 1h','1 reply · last reply 2d',
  ]);
  expect(await link(missing.id).locator('time').count()).toBe(0);
  const time=link(future.id).locator('time');
  expect(await time.getAttribute('datetime')).toBe('2026-01-02T04:40:15.000Z');
  expect(await time.getAttribute('title')).toBe('1/2/2026, 4:40:15 AM');
  const {width,height}=await link(missing.id).boundingBox();
  expect(Math.min(width,height)).toBeGreaterThanOrEqual(44);
},{viewport:{width:360,height:787},deviceScaleFactor:3,hasTouch:true,isMobile:true,locale:'en-US'}));

test('a thread link ages in place on render and comes back to the same scroll anchor',()=>withChat(browser,async(page,server)=>{
  const note=(body,seconds,fields)=>server.fake.inject('demo',body,{posted_at:at(seconds),...fields});
  const root=note('Build notes',-480);
  note('First reply',-420,{thread:root.id});
  note('Second reply',-350,{thread:root.id});
  const last=note('Third reply',-321,{thread:root.id});
  let tail;
  for(let i=0;i<25;i++) tail=note(`Later message that wraps at phone width to leave room to restore an anchor: ${i}`,i);
  await page.clock.setFixedTime(new Date(baseTime));
  await openChannel(page,server);
  await waitForPost(page,tail.id);await frame(page);
  const link=page.locator(`${postSelector(root.id)} .reply-count`);
  const anchor=await localOffset(page,root.id,24);await frame(page);
  expect(await link.textContent()).toBe('3 replies · last reply 5m');
  await link.evaluate(el=>{window.savedThreadLink=el;el.focus({preventScroll:true});});
  await page.clock.setFixedTime(new Date(at(60)));
  expect(await link.textContent()).toBe('3 replies · last reply 5m');
  await page.locator('#channel-form textarea').evaluate(el=>{el.value='A draft';el.dispatchEvent(new Event('input',{bubbles:true}));});
  expect(await link.textContent()).toBe('3 replies · last reply 6m');
  expect(await link.evaluate(el=>el===window.savedThreadLink&&document.activeElement===el)).toBe(true);
  await link.tap();await waitForPost(page,last.id,'replies');
  await page.goBack();await dom(page,'#app[data-pane="channel"]');await frame(page);
  expect(Math.abs(await localOffset(page,root.id)-anchor)).toBeLessThanOrEqual(2);
},{viewport:{width:360,height:787},deviceScaleFactor:3,hasTouch:true,isMobile:true}));

test('an inline DM quote breaks both sides of a same-author run',()=>withChat(browser,async(page,server)=>{
  const direct='demo/lead+owner', add=(body,thread)=>server.fake.inject(direct,body,{thread:thread??null,posted_at:at(0)});
  const root=add('Quote source'), before=add('Ordinary DM'), inline=add('Inline reply',root.id);
  const after=add('DM after the quote'), cont=add('Same group after the quote');
  await openChannel(page,server,direct);
  await dom(page,`${postSelector(inline.id)} .quote`);
  await visibleHeaders(page,[root,before,inline,after,cont],[true,true,true,true,false]);
}));

test.each(['action','author'])('Load older regroups in place with stable anchor and %s focus',focus=>withChat(browser,async(page,server)=>{
  const note=(text,seconds,fields)=>server.fake.inject('demo',text,{posted_at:at(seconds),...fields});
  const previous=note('Earlier neighbor',0), head=note('First loaded row',30,{session_id:'fixture-session'});
  for(let i=0;i<49;i++) note(`Page filler ${i}`,31+i);
  await openChannel(page,server);
  const row=await waitForPost(page,head.id);
  expect(await page.locator(postSelector(previous.id)).count()).toBe(0);
  expect(await meta(page,head.id).isVisible()).toBe(true);
  await frame(page);const anchor=await localOffset(page,head.id,-8);await frame(page);
  const control=focus==='author'?row.locator('.author'):trigger(row);
  await control.evaluate(el=>{
    window.savedControl=el;window.savedRow=el.closest('article');window.savedAvatar=window.savedRow.querySelector('.avatar');
    el.focus({preventScroll:true});
  });
  await page.locator('#older').evaluate(el=>el.click());
  await waitForPost(page,previous.id);await frame(page);
  expect(await meta(page,head.id).isVisible()).toBe(false);
  expect(await avatar(page,head.id).isVisible()).toBe(false);
  expect(await avatar(page,previous.id).isVisible()).toBe(true);
  expect(await row.evaluate((el,kind)=>el===window.savedRow&&el.querySelector('.avatar')===window.savedAvatar&&
    document.activeElement===(kind==='author'?el:window.savedControl)&&el.contains(window.savedControl),focus)).toBe(true);
  expect(Math.abs(await localOffset(page,head.id)-anchor)).toBeLessThanOrEqual(2);
  if(focus==='author') return;
  const more=note('Watch append while reading',90);await waitForPost(page,more.id);await frame(page);
  expect(Math.abs(await localOffset(page,head.id)-anchor)).toBeLessThanOrEqual(2);
  await page.keyboard.press('Enter');await replyAction(page).click();await waitForPost(page,head.id,'replies');
  await page.goBack();await dom(page,'#app[data-pane="channel"]');await frame(page);
  expect(Math.abs(await localOffset(page,head.id)-anchor)).toBeLessThanOrEqual(2);
  await page.locator('#channel-log').evaluate(el=>{el.scrollTop=el.scrollHeight;});await frame(page);
  const docked=note('Watch append while docked',91);await waitForPost(page,docked.id);await frame(page);
  expect(await bottom(page)).toBeLessThanOrEqual(2);
}));

// Observe outgoing frames synchronously in the page; once the read/paint frame
// runs, a negative assertion doesn't depend on the host delivering WS data.
function captureRequests(page) {
  return page.addInitScript(()=>{
    window.messageRequests=[];
    const send=WebSocket.prototype.send;
    WebSocket.prototype.send=function(raw){
      send.call(this,raw);
      const f=JSON.parse(raw);
      window.messageRequests.push(f);
      window.dispatchEvent(new Event('message-request'));
    };
  });
}
const waitRequest=(page,method,through,since=0)=>page.evaluate(([method,through,since])=>new Promise(resolve=>{
  function check(){
    const f=window.messageRequests.slice(since).find(f=>f.method===method&&(!through||f.params.through===through));
    if(f){window.removeEventListener('message-request',check);resolve(f);}
  }
  window.addEventListener('message-request',check);check();
}),[method,through,since]);
const seen=(page,id)=>page.evaluate(id=>window.messageRequests.some(f=>f.method==='channel.seen'&&f.params.through===id),id);
async function holdHistory(page,match) {
  let done, request;
  const held=new Promise(resolve=>{done=resolve;});
  await page.routeWebSocket('**/ws',ws=>{
    const upstream=ws.connectToServer();
    ws.onMessage(raw=>{const f=JSON.parse(raw);if(f.method==='channel.history'&&match(f.params)) request=f.id;upstream.send(raw);});
    upstream.onMessage(raw=>{
      if(request&&JSON.parse(raw).id===request) {request=undefined;done(()=>ws.send(raw));}
      else ws.send(raw);
    });
  });
  return {held};
}

test('only a visible continuation sentinel sends its own seen ID, even with opened code',()=>withChat(browser,async(page,server)=>{
  const last=server.fake.inject('demo','Previous row',{posted_at:at(0)});
  let hold=false;
  const {held}=await holdHistory(page,p=>hold&&p.after===last.id);
  await captureRequests(page);
  await openChannel(page,server);await waitForPost(page,last.id);await waitRequest(page,'channel.seen',last.id);
  const first=await page.locator('#posts article').first().getAttribute('data-post-id');
  await localOffset(page,first,0);await frame(page);
  hold=true;
  const report=server.fake.inject('demo','```\n'+Array(55).fill('Building the example').join('\n')+'\n```',{posted_at:at(1)});
  const release=await held;hold=false;
  expect(await page.locator(postSelector(report.id)).count()).toBe(0);
  release();
  const row=await waitForPost(page,report.id);await frame(page);
  expect(await meta(page,report.id).isVisible()).toBe(false);
  expect(await seen(page,report.id)).toBe(false);
  await row.locator('details').evaluate(el=>{el.open=true;});
  await localOffset(page,report.id,30);await frame(page);
  expect(await row.locator('[data-end]').evaluate(el=>el.getBoundingClientRect().top>document.getElementById('channel-log').getBoundingClientRect().bottom)).toBe(true);
  expect(await seen(page,report.id)).toBe(false);
  await page.locator('#channel-log').evaluate(el=>{el.scrollTop=el.scrollHeight;});
  const request=await waitRequest(page,'channel.seen',report.id);
  expect(request.params).toEqual({channel:'demo',through:report.id});
}));

test('a held thread page does not mark a reply seen while the thread is hidden',()=>withChat(browser,async(page,server)=>{
  const root=server.fake.inject('demo','Held thread',{posted_at:at(0)});
  server.fake.inject('demo','First reply',{thread:root.id,posted_at:at(1)});
  const reply=server.fake.inject('demo','Grouped reply',{thread:root.id,posted_at:at(2)});
  let hold=true;
  const {held}=await holdHistory(page,p=>hold&&p.thread===root.id);
  await captureRequests(page);await openChannel(page,server);await waitRequest(page,'channel.seen',root.id);
  await openThread(page,root.id);const release=await held;hold=false;
  await page.goBack();await dom(page,'#app[data-pane="channel"]');
  const since=await page.evaluate(()=>window.messageRequests.length);
  release();await waitRequest(page,'channel.list',undefined,since);await frame(page);
  expect(await seen(page,reply.id)).toBe(false);
  await openThread(page,root.id);await waitForPost(page,reply.id,'replies');
  expect(await meta(page,reply.id,'replies').isVisible()).toBe(false);
  expect((await waitRequest(page,'channel.seen',reply.id)).params.through).toBe(reply.id);
}));

import assert from 'node:assert/strict';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { cpSync, mkdirSync, mkdtempSync, appendFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import { touchDrag } from './touch.mjs';

if(process.argv[2]==='--real') {
  const {realSmoke}=await import('./real-smoke.mjs');
  await realSmoke(process.argv.slice(3));
} else {
const {rig}=await import('./rig.mjs');
const dir=resolve(process.env.ATM_CHAT_TEST_DIST ?? 'dist');
const {chromium}=await import(pathToFileURL(process.env.ATM_CHAT_TEST_PLAYWRIGHT));
const server=await rig(dir);
const widths=[[412,915],[840,760],[1897,1973],[3840,2160],[412,915]];
let browser;
const external=[], policyViolations=[];

function dom(page,selector,includes='') {
  return page.evaluate(([sel,text])=>new Promise(resolve=>{
    const observer=new MutationObserver(check);
    function check(){ if([...document.querySelectorAll(sel)].some(e=>e.textContent.includes(text))) {observer.disconnect(); resolve();} }
    observer.observe(document,{childList:true,subtree:true,characterData:true,attributes:true}); check();
  }),[selector,includes]);
}
const frame = page=>page.evaluate(()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(()=>r()))));
const current = page=>dom(page,'#app[data-phase="live"]');
const note = (page,selector,text) => dom(page,`${selector} article`,text);
function calls(method) {return server.frames.filter(f=>f.method===method).map(f=>f.params);}
const choose=(scope,name)=>scope.getByRole('button',{name,exact:true});
const findCard=(page,id)=>page.locator(`#cards [data-key="${id}"]`);
const modal=page=>page.getByRole('dialog');
const closed=page=>dom(page,'dialog:not([open])');
const actionTrigger=row=>choose(row,'Message actions');
const actionSheet=page=>page.getByRole('dialog',{name:'Message actions'});
async function messageActions(page,row) {await actionTrigger(row).press('Enter');return actionSheet(page);}
const pane = page=>page.locator('#app').getAttribute('data-pane');
const depth = page=>page.evaluate(()=>history.state?.depth);
async function home(page) {
  assert.equal(await pane(page),'home','ordinary reload must start at Home');
  await page.evaluate(()=>history.state?.depth===0 || new Promise(resolve=>addEventListener('popstate',()=>resolve(true),{once:true})));
  assert.equal(await depth(page),0,'Home belongs at the bottom of the app history');
}
async function shot(page,name) {
  const dir=process.env.ATM_CHAT_TEST_SHOTS;
  if(dir){mkdirSync(dir,{recursive:true});await page.screenshot({path:join(dir,`${name}.png`)});}
}
const interior=(page,port,id,move=false)=>page.evaluate(async([port,id,move])=>{
  await document.fonts.ready;
  const log=document.getElementById(port), row=log.querySelector(`[data-scroll-id="${id}"]`);
  const delta=()=>row.getBoundingClientRect().top-log.getBoundingClientRect().top;
  if(move) log.scrollTop+=delta()+12;
  return delta();
},[port,id,move]);
const activation=async(page,locator)=>{
  await locator.evaluate(b=>b.focus({preventScroll:true}));await page.keyboard.press('Enter');
};

async function historySmoke() {
  const context=await browser.newContext({viewport:{width:412,height:915},hasTouch:true,deviceScaleFactor:1});
  try {
    const page=await context.newPage();await online(context,page);
    const prior=`${server.origin}/before-chat`;
    await page.route(prior,r=>r.fulfill({contentType:'text/html',body:'<a href="/">Launch chat</a>'}));
    await page.goto(prior);await page.getByRole('link').click();await current(page);
    const before=await page.evaluate(()=>history.length);
    const fold=page.getByText('Behind the scenes · #demo',{exact:true});
    await fold.tap();assert.equal(await page.evaluate(()=>history.length),before,'expanding a group must not navigate');
    const demo=page.locator('#channels [data-channel="demo"]');
    await shot(page,'after-412-home');
    await demo.tap();
    assert.equal(await page.evaluate(()=>history.length),before+1,'opening a channel must push a browser entry');
    await note(page,'#channel-log','Literal <b>sample</b>');
    if(await page.locator('#older').isVisible()) await page.locator('#older').click();
    const root=page.locator('#posts article',{hasText:'Build notes: sample thread'});
    await root.waitFor();
    const openThread=root.getByRole('button',{name:/^\d+ replies/});
    const source=page.locator('#posts article',{hasText:'Channel post 15.'});
    const anchor=await source.getAttribute('data-post-id');
    const draft=page.locator('#channel-form textarea'), reply=page.locator('#thread-form textarea');
    await draft.fill('Channel draft across Back');
    await draft.evaluate(el=>el.setSelectionRange(2,9));
    const channelOffset=await interior(page,'channel-log',anchor,true);
    await shot(page,'after-412-channel');
    await activation(page,openThread);await note(page,'#thread-log','Thread reply 70.');
    assert.equal(await depth(page),2);
    await reply.fill('Thread draft across Forward');await shot(page,'after-412-thread');
    const docs=()=>server.http.filter(r=>r.url==='/').length;
    const beforeBack=docs();
    await page.goBack();await frame(page);
    assert.equal(await pane(page),'channel');assert.equal(await depth(page),1);
    assert.equal(await draft.inputValue(),'Channel draft across Back');
    assert.deepEqual(await draft.evaluate(el=>[el.selectionStart,el.selectionEnd]),[2,9]);
    assert.ok(Math.abs(await interior(page,'channel-log',anchor)-channelOffset)<=2,'Back lost the anchor');
    await shot(page,'after-412-back');
    await page.goBack();assert.equal(await pane(page),'home');
    assert.equal(await fold.evaluate(el=>el.parentElement.open),true);
    assert.equal(page.url(),`${server.origin}/`);assert.equal(docs(),beforeBack);
    await page.goBack();assert.equal(page.url(),prior,'native Back at Home must leave the app');
    await page.goForward();await current(page);await home(page);
    await demo.tap();await note(page,'#channel-log','Literal <b>sample</b>');
    if(await page.locator('#older').isVisible()) await page.locator('#older').click();
    await openThread.tap();await note(page,'#thread-log','Thread reply 70.');
    for(const width of [600,840,1100,1897]) {
      await page.setViewportSize({width,height:900});await frame(page);
      assert.equal(await depth(page),2,'resize must not navigate');
      if(width===1897){assert.equal(await page.locator('#channel-log').isVisible(),true);await shot(page,'after-1897-thread');}
      await page.goBack();assert.equal(await pane(page),'channel');
      assert.equal(await page.locator('#app').getAttribute('data-thread'),'false');
      if(width===840||width===1897) await shot(page,`after-${width}-back`);
      await page.goForward();assert.equal(await pane(page),'thread');
    }
    await page.goBack();
    const linked=server.fake.inject('demo','Same-channel linked question',{kind:'question'});
    const card=server.fake.ask('Open in the current channel',false,{channel:'demo',post_id:linked.id});
    const open=findCard(page,card).getByRole('button',{name:'Open conversation'});
    await open.waitFor();
    const beforeLink=await interior(page,'channel-log',anchor,true);
    await open.click();await dom(page,`#posts [data-post-id="${linked.id}"][data-focus=true]`);
    assert.equal(await depth(page),2,'same-channel Open is an entry with its own anchor');
    await page.goBack();await frame(page);
    assert.equal(await pane(page),'channel');
    assert.ok(Math.abs(await interior(page,'channel-log',anchor)-beforeLink)<=2,'Back lost the pre-link anchor');
    await activation(page,openThread);await note(page,'#thread-log','Thread reply 70.');
    await page.reload();await current(page);await home(page);
    await page.goForward();await home(page);
    await page.goBack();assert.equal(page.url(),prior,'reload must not leave old views in the Back path');
    await page.goForward();await current(page);await home(page);
    await demo.tap();
    await page.addInitScript(()=>{
      const go=history.go.bind(history);
      history.go=steps=>{window.finishRewind=()=>go(steps);};
    });
    await page.reload();await current(page);
    assert.equal(await depth(page),1,'hold the reload rewind at its original slot');
    const direct=page.locator(`#channels [data-channel="${directId}"]`);
    await demo.tap();await direct.tap();
    assert.equal(await pane(page),'home','no tap may navigate before popstate');
    await page.evaluate(()=>new Promise(resolve=>{
      addEventListener('popstate',()=>resolve(),{once:true});
      window.finishRewind();
    }));
    assert.equal(await pane(page),'channel','apply the last tap on popstate');
    assert.equal(await page.evaluate(()=>history.state.view.channel),directId);
    assert.equal(await depth(page),1,'the earlier tap must not create a Back entry');
    await page.goBack();await home(page);
    await page.goBack();assert.equal(page.url(),prior,'a queued tap must preserve native root exit');
    console.log('navigation: native Back/Forward, draft/caret and channel anchor, 600/840/1100/1897 fold, reload/root exit, last tap after popstate');
  } finally {await context.close();}
}

const composerInput=(page,kind='channel')=>page.locator(`#${kind}-form textarea`);
const composerButton=(page,kind='channel')=>page.locator(`#${kind}-form button[type=submit]`);
const normalChannel=page=>page.locator('#channels [data-channel="demo"]');
const composeSize=input=>input.evaluate(e=>({height:e.clientHeight,content:e.scrollHeight,top:e.scrollTop}));
const sixLines='One\nTwo\nThree\nFour\nFive\nSix';
const selection=input=>input.evaluate(e=>[e.selectionStart,e.selectionEnd]);
const composeStatus=(page,copy)=>dom(page,'#channel-form [role=status]:not([hidden])',copy);

async function openComposerThread(page) {
  const row=page.locator('#posts article',{hasText:'Build notes: sample thread'});
  if(!await row.count()) await page.locator('#older').click();
  await dom(page,'#posts article','Build notes: sample thread');
  await row.locator('.reply-count').click();await note(page,'#thread-log','Thread reply 1.');
}

async function composerSmoke() {
  const context=await browser.newContext({viewport:{width:412,height:915},hasTouch:true});
  try {
    const page=await context.newPage();await online(context,page);
    await page.goto(server.origin);await current(page);await normalChannel(page).tap();
    await note(page,'#posts','Literal <b>sample</b>');
    await page.evaluate(()=>document.fonts.ready.then(()=>true));
    const draft=composerInput(page), send=composerButton(page), reply=composerInput(page,'thread');
    assert.equal(await draft.getAttribute('placeholder'),'Message #demo');
    assert.equal(await page.getByRole('textbox',{name:'Message #demo',exact:true}).count(),1);
    assert.equal(await send.isDisabled(),true,'a live blank draft has a disabled arrow');
    const base=await composeSize(draft);
    const inactive=await send.evaluate(e=>getComputedStyle(e).backgroundColor);
    await draft.fill(' \t\n');assert.equal(await send.isDisabled(),true);
    await draft.fill('One');assert.equal(await send.isEnabled(),true);
    assert.equal((await composeSize(draft)).height,base.height);
    const surface=await send.evaluate(e=>{
      const b=e.getBoundingClientRect(), box=e.parentElement.getBoundingClientRect(), text=e.form.querySelector('textarea');
      return {inside:b.right<box.right&&b.bottom<box.bottom&&text.getBoundingClientRect().right<=b.left,
        w:b.width,h:b.height,resize:getComputedStyle(text).resize,bg:getComputedStyle(e).backgroundColor,
        icon:!!e.querySelector('svg[aria-hidden=true]')};
    });
    assert.equal(surface.inside,true);assert.equal(surface.w,44);assert.equal(surface.h,44);
    assert.equal(surface.resize,'none');assert.equal(surface.icon,true);
    assert.notEqual(surface.bg,inactive);assert.equal(surface.bg,'rgb(240, 181, 74)');
    await draft.press('Tab');assert.equal(await send.evaluate(e=>e.matches(':focus-visible')),true);
    await page.emulateMedia({forcedColors:'active'});await draft.focus();
    assert.ok(await draft.evaluate(e=>parseFloat(getComputedStyle(e.parentElement).outlineWidth)>0));
    await page.emulateMedia({forcedColors:'none'});

    const anchor=await page.locator('#posts article',{hasText:'Channel post 45.'}).getAttribute('data-post-id');
    const anchorAt=await interior(page,'channel-log',anchor,true);await frame(page);
    await draft.fill('One\n');assert.ok((await composeSize(draft)).height>base.height,'a trailing newline grows the box');
    await draft.fill(sixLines);
    const atSix=await composeSize(draft);
    assert.ok(atSix.height>base.height*2);assert.ok(atSix.content<=atSix.height+2);
    assert.ok(Math.abs(await interior(page,'channel-log',anchor)-anchorAt)<=2,'growth moved the reading anchor');
    await draft.fill(sixLines+'\nSeven\nEight');await draft.press('Control+End');await draft.press('Enter');await draft.press('x');
    const capped=await composeSize(draft), length=(await draft.inputValue()).length;
    const padding=await draft.evaluate(e=>parseFloat(getComputedStyle(e).paddingBottom));
    assert.ok(Math.abs(capped.height-atSix.height)<=2 && capped.content>capped.height);
    assert.deepEqual(await selection(draft),[length,length]);
    // The native caret reveal only has to show the text, not scroll to the end of the bottom padding.
    assert.ok(capped.top>0&&capped.top+capped.height>=capped.content-padding-2,'typing at the cap keeps the last line reachable');
    await draft.fill('');assert.equal((await composeSize(draft)).height,base.height);
    assert.ok(Math.abs(await interior(page,'channel-log',anchor)-anchorAt)<=2,'shrink moved the reading anchor');
    const log=page.locator('#channel-log');await log.evaluate(e=>{e.scrollTop=e.scrollHeight;});await frame(page);
    const soft='Wrap these words. '.repeat(4).trim();
    await draft.fill(soft);const wrapped=(await composeSize(draft)).height;
    assert.ok(wrapped>base.height,'soft wraps grow the box');
    assert.ok(await log.evaluate(e=>e.scrollHeight-e.scrollTop-e.clientHeight<=2),'growth lost the bottom dock');
    await draft.evaluate(e=>e.setSelectionRange(2,9));await frame(page);
    await openComposerThread(page);
    assert.equal(await reply.getAttribute('placeholder'),'Reply in thread');
    assert.equal(await page.getByRole('textbox',{name:'Reply in thread',exact:true}).count(),1);
    assert.equal(await composerButton(page,'thread').isDisabled(),true);
    await reply.fill(sixLines);await reply.evaluate(e=>e.setSelectionRange(1,5));await frame(page);
    await page.setViewportSize({width:840,height:900});await frame(page);
    assert.ok((await composeSize(reply)).height>base.height*2);
    await page.setViewportSize({width:1897,height:1100});await frame(page);await frame(page);
    assert.ok((await composeSize(draft)).height<wrapped,'width-only resize did not refit the soft wrap');
    assert.equal(await draft.inputValue(),soft);assert.deepEqual(await selection(draft),[2,9]);
    const other=await composeSize(draft);await reply.fill('Thread only');
    assert.deepEqual(await composeSize(draft),other,'editing the thread does not resize the channel composer');
    await reply.fill(sixLines);await reply.evaluate(e=>e.setSelectionRange(1,5));await frame(page);
    await page.setViewportSize({width:412,height:915});await frame(page);await page.goBack();await frame(page);
    assert.equal(await draft.inputValue(),soft);assert.deepEqual(await selection(draft),[2,9]);
    assert.equal((await composeSize(draft)).height,wrapped,'Back made the narrow textarea visible without refitting it');
    await page.goForward();assert.equal(await reply.inputValue(),sixLines);assert.deepEqual(await selection(reply),[1,5]);
    await page.goBack();await page.goBack();
    await page.locator('#channels [data-channel="demo/lead+owner"]').tap();
    await note(page,'#posts','An ordinary direct message.');
    assert.equal(await page.getByRole('textbox',{name:'Message demo lead',exact:true}).count(),1);
    assert.equal(await draft.getAttribute('placeholder'),'Message demo lead');assert.equal(await send.isDisabled(),true);
    await page.goBack();await page.getByText('Behind the scenes · #demo',{exact:true}).tap();
    await page.locator('#channels [data-channel="demo/lead+demo/worker"]').tap();
    await dom(page,'#channel-readonly:not([hidden])','This conversation is read only.');
    assert.equal(await draft.isVisible(),false);assert.equal(await send.isVisible(),false);
    console.log('composer: 412/840/1897 fit, soft wraps and cap, caret/anchors, labels, touch target and forced-colors focus');
  } finally {await context.close();}
}

async function composerStateSmoke() {
  const context=await browser.newContext({viewport:{width:412,height:915},hasTouch:true});
  try {
    const page=await context.newPage();await online(context,page);
    const gated=Promise.withResolvers(), release=Promise.withResolvers(), confirmed=Promise.withResolvers(), requests=[];
    const unknown='An ambiguous composer send';
    let held;
    await page.routeWebSocket('**/ws',route=>{
      const ws=route.connectToServer();
      route.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.method==='channel.post') {
          requests.push(f);
          if(f.params.body==='Hold this receipt') {
            held=f;gated.resolve();void release.promise.then(()=>ws.send(raw));return;
          }
          if(f.params.body==='A definite refusal') {
            route.send(JSON.stringify({type:'response',id:f.id,error:{code:'invalid_params',message:'Fixture refusal before forwarding'}}));return;
          }
        }
        ws.send(raw);
      });
      ws.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(held&&f.id===held.id) confirmed.resolve(f.result.post_id);
        if(f.kind==='channel.posted'&&requests.some(r=>r.params.body===unknown&&r.params.client_key===f.body?.client_key)) return;
        route.send(raw);
      });
      ws.onClose((code,reason)=>{void route.close({code:code===1006||code===1005?undefined:code,reason});});
    });
    await page.goto(server.origin);await current(page);await normalChannel(page).tap();await note(page,'#posts','Literal <b>sample</b>');
    await page.evaluate(()=>document.fonts.ready.then(()=>true));
    const draft=composerInput(page), send=composerButton(page);
    const footer=()=>page.locator('#channel-form').evaluate(e=>[e.offsetTop,e.offsetHeight]);
    await draft.fill('Hold this receipt');const quiet=await footer();await send.tap();
    await gated.promise;
    await dom(page,'#posts .outbox','Sending…');assert.equal(await send.isDisabled(),true, 'blank composer');
    assert.equal(await draft.inputValue(),'');assert.equal(await draft.getAttribute('readonly'),null);
    assert.deepEqual(await footer(),quiet,'Sending shifted the footer');
    await shot(page,'after-412-channel-sending');
    await page.goBack();await page.locator('#channels [data-channel="demo/lead+owner"]').tap();
    await note(page,'#posts','An ordinary direct message.');await draft.fill('Keep this DM draft');
    release.resolve();const sentId=await confirmed.promise;
    await page.goBack();await normalChannel(page).tap();await dom(page,`#posts [data-post-id="${sentId}"]`);
    assert.equal(await draft.inputValue(),'');assert.deepEqual(await footer(),quiet);
    await page.goBack();await page.locator('#channels [data-channel="demo/lead+owner"]').tap();
    assert.equal(await draft.inputValue(),'Keep this DM draft');await page.goBack();await normalChannel(page).tap();
    await draft.fill('A definite refusal');await send.tap();await dom(page,'#posts .outbox','Not sent');
    assert.equal(await draft.inputValue(),'');assert.deepEqual(await footer(),quiet,'failure shifted the footer');
    const refused=page.locator('#posts .outbox',{hasText:'A definite refusal'});
    const sheet=await messageActions(page,refused);
    assert.equal(await choose(sheet,'Retry').isEnabled(),true);
    await choose(sheet,'Delete').tap();assert.equal(await refused.count(),0);
    await draft.fill('x'.repeat(16_385));const writes=requests.length;
    await send.tap();await composeStatus(page,'Message is over 16 KiB.');assert.equal(requests.length,writes);
    const dropped=Promise.withResolvers(), retried=Promise.withResolvers();
    const receive=server.control.drop;
    server.control.drop=(f,result)=>{
      if(f.method!=='channel.post'||f.params.body!==unknown) return receive?.(f,result);
      if(requests.length===writes+1) {dropped.resolve(result.post_id);return true;}
      retried.resolve(result.post_id);
    };
    await draft.fill(unknown);await send.tap();const lostPost=await dropped.promise;
    assert.ok(lostPost);assert.equal(await retried.promise,lostPost);await current(page);
    await dom(page,`#posts [data-post-id="${lostPost}"]`,unknown);
    assert.deepEqual(requests.at(-1).params,requests.at(-2).params);
    assert.equal(await page.locator('#posts article',{hasText:unknown}).count(),1);
    assert.equal(await draft.inputValue(),'');assert.deepEqual(await footer(),quiet);
    await draft.fill('Offline draft');await context.setOffline(true);await dom(page,'#app[data-phase=offline]');
    assert.equal(await send.isEnabled(),true);await send.tap();await dom(page,'#posts .outbox','Offline draft');
    assert.equal(await draft.inputValue(),'');assert.equal(requests.length,writes+2);
    console.log('composer states: held request, scoped drafts, refusal/delete, limit, same-key reconnect, offline queue');
  } finally {delete server.control.drop;await context.close();}
}

async function composerKeys() {
  const fine=await browser.newContext({viewport:{width:412,height:915}});
  try {
    const page=await fine.newPage();await online(fine,page);
    const arrived=Promise.withResolvers(), release=Promise.withResolvers(), sent=[];
    await page.routeWebSocket('**/ws',route=>{
      const ws=route.connectToServer();
      route.onMessage(raw=>{
        const f=JSON.parse(String(raw));if(f.method==='channel.post') sent.push(f);
        ws.send(raw);
      });
      ws.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(sent[0]&&f.id===sent[0].id) {arrived.resolve(f);void release.promise.then(()=>route.send(raw));return;}
        route.send(raw);
      });
    });
    await page.goto(server.origin);await current(page);await normalChannel(page).click();
    const draft=composerInput(page);
    assert.equal(await page.evaluate(()=>navigator.maxTouchPoints),0);
    await draft.fill('Desktop');await draft.press('Shift+Enter');await page.keyboard.insertText('draft');
    const body='Desktop\ndraft';assert.equal(await draft.inputValue(),body);
    assert.equal(await draft.evaluate(e=>e.dispatchEvent(new KeyboardEvent('keydown',{
      key:'Enter',isComposing:true,bubbles:true,cancelable:true,
    }))),true,'IME confirmation was intercepted');
    await page.keyboard.down('Enter');
    assert.equal(await draft.inputValue(),'','desktop Enter queues the text and clears the composer');
    const response=await arrived.promise;
    assert.equal(response.id,sent[0].id);assert.ok(response.result.post_id);
    assert.equal(sent[0].params.channel,'demo');assert.equal(sent[0].params.body,body);
    await page.keyboard.down('Enter');await page.keyboard.up('Enter');
    assert.equal(sent.length,1);assert.equal(await draft.inputValue(),'');
    release.resolve();
    await dom(page,`#posts [data-post-id="${response.result.post_id}"]`);
    assert.equal(await draft.inputValue(),'');
    await draft.press('Enter');assert.equal(await draft.inputValue(),'');assert.equal(sent.length,1);
  } finally {await fine.close();}
  const touch=await browser.newContext({viewport:{width:1897,height:1100},hasTouch:true});
  try {
    const page=await touch.newPage();await online(touch,page);
    await page.goto(server.origin);await current(page);await normalChannel(page).tap();
    await note(page,'#posts','Literal <b>sample</b>');await openComposerThread(page);
    assert.ok(await page.evaluate(()=>navigator.maxTouchPoints>0));
    const reply=composerInput(page,'thread'), before=calls('channel.post').length;
    await reply.fill('Touch draft');await reply.press('Enter');
    assert.equal(await reply.inputValue(),'Touch draft\n','wide touch Return stays a newline');
    assert.equal(calls('channel.post').length,before);
    const root=await page.locator('#replies article').first().getAttribute('data-post-id');
    await composerButton(page,'thread').tap();await dom(page,'#replies [data-post-id]','Touch draft');
    const sent=calls('channel.post').slice(before);
    assert.equal(sent.length,1);assert.equal(sent[0].reply_to,root);assert.equal(sent[0].channel,undefined);
    assert.equal(sent[0].body,'Touch draft\n');
  } finally {await touch.close();}
  console.log('composer keys: narrow fine-pointer Return, Shift+Return, IME/repeat, wide touch newline and thread arrow');
}

async function pullSmoke() {
  const context=await browser.newContext({viewport:{width:412,height:915},hasTouch:true,deviceScaleFactor:1});
  try {
    const page=await context.newPage();await online(context,page);
    const cdp=await context.newCDPSession(page), requests=[];let fence, omitWatch=false;
    await page.routeWebSocket('**/ws',route=>{
      const ws=route.connectToServer(), pending=new Map();
      route.onMessage(raw=>{const f=JSON.parse(String(raw));if(f.method){pending.set(f.id,f);requests.push(f);}ws.send(raw);});
      ws.onMessage(raw=>{
        const f=JSON.parse(String(raw)), req=pending.get(f.id);pending.delete(f.id);
        if(omitWatch&&f.type==='event') return;
        if(fence&&!fence.held&&req?.method==='channel.history') {
          fence.held=true;fence.response.resolve([req,f]);void fence.release.promise.then(()=>route.send(raw));return;
        }
        route.send(raw);
      });
    });
    await page.goto(server.origin);await current(page);
    const label=page.locator('#connection-label'), draft=page.locator('#channel-form textarea');
    const box=id=>page.locator(`#${id}`).boundingBox();
    const channelTop=()=>page.locator('#channel-log').evaluate(el=>{el.scrollTop=0;});
    const docs=()=>server.http.filter(r=>r.url==='/').length;
    const start=docs();
    async function positive(id,expectedPane,file) {
      const port=page.locator(`#${id}`);
      await port.evaluate(el=>{el.scrollTop=0;});await frame(page);
      assert.equal(await port.evaluate(el=>el.scrollTop),0,'a positive pull starts at the top');
      const {x,y,width}=await box(id), at=requests.length, depthBefore=await depth(page);
      fence={response:Promise.withResolvers(),release:Promise.withResolvers()};
      await touchDrag(cdp,x+width/2,y+60,0,102);
      await dom(page,'#connection-label','Refreshing…');
      const [req,result]=await fence.response.promise;
      assert.equal(req.id,result.id);assert.equal(req.params.channel,'demo');
      assert.ok(requests.slice(at).some(r=>r.method==='channel.list'));
      assert.equal(await depth(page),depthBefore);assert.equal(await pane(page),expectedPane);
      await shot(page,file);
      fence.release.resolve();await dom(page,'#connection-label','Live');fence=undefined;
      assert.equal(docs(),start,'pull must not fetch a document');
      assert.ok(!requests.slice(at).some(r=>['channel.post','decision.resolve','decision.withdraw'].includes(r.method)));
    }
    await positive('side-log','home','after-412-home-pull');
    await page.locator('#channels [data-channel="demo"]').tap();
    await note(page,'#channel-log','Literal <b>sample</b>');
    await draft.fill('Unsent text across pull');await draft.evaluate(el=>el.setSelectionRange(1,7));
    await channelTop();await frame(page);
    const anchor=await page.locator('#posts article').first().getAttribute('data-post-id');
    const offset=await interior(page,'channel-log',anchor);
    omitWatch=true;server.fake.inject('demo','A post available only after pull');
    assert.equal(await page.locator('#posts .body',{hasText:'A post available only after pull'}).count(),0);
    await positive('channel-log','channel','after-412-channel-pull');
    await note(page,'#channel-log','A post available only after pull');
    assert.equal(await draft.inputValue(),'Unsent text across pull');
    assert.deepEqual(await draft.evaluate(el=>[el.selectionStart,el.selectionEnd]),[1,7]);
    assert.ok(Math.abs(await interior(page,'channel-log',anchor)-offset)<=2);
    await page.evaluate(()=>{
      window.refreshTurns=0;
      new MutationObserver(()=>{if(document.getElementById('connection-label').textContent==='Refreshing…') window.refreshTurns++;})
        .observe(document.getElementById('connection-label'),{childList:true});
    });
    const {x,y,width}=await box('channel-log'), center=x+width/2, startY=y+72;
    await touchDrag(cdp,center,startY,0,36);
    await touchDrag(cdp,center,startY,85,26);
    await touchDrag(cdp,center,startY,75,75);
    await touchDrag(cdp,x+8,startY,0,108);
    await touchDrag(cdp,center,startY,0,108,{multi:true});
    await touchDrag(cdp,center,startY,0,108,{cancel:true});
    await touchDrag(cdp,center,startY,0,108,{retreat:true});
    await touchDrag(cdp,center,startY,0,-140);await frame(page);
    assert.ok(await page.locator('#channel-log').evaluate(el=>el.scrollTop>0),'ordinary vertical scroll must still work');
    await touchDrag(cdp,center,startY,0,93); // Starts below the top.
    const compose=await draft.boundingBox();
    await touchDrag(cdp,compose.x+30,compose.y+10,0,100);
    await channelTop();await frame(page);
    await touchDrag(cdp,center,startY,0,108,{beforeEnd:async()=>{await page.goBack();}});
    assert.equal(await pane(page),'home');
    assert.equal(await page.evaluate(()=>window.refreshTurns),0,'unqualified touches must not start a refresh');
    await page.goForward();await channelTop();await frame(page);
    await context.setOffline(true);await dom(page,'#connection-banner:not([hidden])',"You're offline");
    const count=requests.length;
    await touchDrag(cdp,center,startY,0,102);await frame(page);
    assert.equal(await label.textContent(),'Offline');assert.equal(requests.length,count);assert.equal(docs(),start);
    await shot(page,'after-412-offline-pull');
    console.log('touch pull: Home/channel, forced reads, held spinner, retained draft/caret/anchor, cancellation and offline');
  } finally {await context.close();}
}

async function quietThreadAction(page,root) {
  const count=root.getByRole('button',{name:/^70 replies/});
  assert.equal(await choose(root,'Reply in thread').count(),0,'no action in the row at rest');
  await count.tap();await note(page,'#thread-log','Thread reply 70.');
  await page.locator('#thread-back').tap();
  await root.locator('.body').tap();
  assert.equal(await actionSheet(page).count(),0,'reply-count navigation and ordinary taps do not open the sheet');
  const action=choose(await messageActions(page,root),'Reply in thread');
  const hit=await action.boundingBox();assert.ok(hit.width>=44&&hit.height>=44,'44px touch target');
  await action.tap();await note(page,'#thread-log','Thread reply 70.');
}

async function fineThreadAction() {
  const context=await browser.newContext({viewport:{width:1897,height:1100}});
  try {
    const page=await context.newPage();await online(context,page);
    await page.goto(server.origin);await current(page);
    const row=page.locator('#posts article',{hasText:'Literal <b>sample</b>'});
    await row.locator('.body').waitFor();
    const trigger=actionTrigger(row);
    await row.locator('.body').hover();
    assert.equal(await trigger.evaluate(el=>getComputedStyle(el).pointerEvents),'none');
    await row.evaluate(el=>el.focus());await page.keyboard.press('Tab');
    assert.equal(await trigger.evaluate(el=>el===document.activeElement&&el.matches(':focus-visible')),true);
    await page.keyboard.press('Enter');
    await choose(actionSheet(page),'Reply in thread').click();await note(page,'#thread-log','Literal <b>sample</b>');
    assert.equal(await page.locator('#channel-log').isVisible(),true,'desktop thread is alongside the channel');
  } finally {await context.close();}
}

async function chat(page) {
  await page.goto(server.origin);
  assert.equal(await page.locator('#app').count(),1,'built app shell is missing');
  await current(page); await frame(page);
  assert.equal(await page.locator('#app').getAttribute('data-pane'),'home');
  assert.equal(calls('channel.seen').length,0,'hidden initial log must not mark anything read');
  const sections=await page.locator('#channels h2').allTextContents();
  assert.deepEqual(sections,['Chief of staff','Leads','Channels','Direct messages']);
  assert.equal(await page.locator('#cards .open').count(),0);
  const titled = title => page.locator('#cards article',{hasText:title});
  const pending=titled('Which sample should we use?'), provisional=titled('Confirm the provisional sample?');
  const [pendingId,provisionalId]=await Promise.all([pending,provisional].map(q=>q.getAttribute('data-key')));
  assert.deepEqual(await provisional.locator('button').allTextContents(),['Keep First sample','Overrule: Second sample']);
  assert.equal(await pending.getByRole('button',{name:'Withdraw',exact:true}).count(),1);
  await pending.getByRole('button',{name:'Second sample',exact:true}).click();
  await dom(page,'#needs-title','(1)');
  await provisional.getByRole('button',{name:'Keep First sample',exact:true}).click();
  await dom(page,'#needs .empty','Nothing needs you right now.');
  const answers=[{id:pendingId,option:'Second sample'},{id:provisionalId,option:'First sample'}];
  assert.deepEqual(calls('decision.resolve'),answers);
  assert.equal(calls('channel.post').length,0);
  await page.locator('#channels button[data-channel="demo"]').click();
  await note(page,'#channel-log','Literal <b>sample</b>');
  await page.locator('#older').click();
  await note(page,'#channel-log','Build notes: sample thread');
  const root=page.locator('#posts article',{hasText:'Build notes: sample thread'});
  const rootId=await root.getAttribute('data-post-id');
  await quietThreadAction(page,root);
  const ta=page.locator('#thread-form textarea');
  await ta.fill('An ordinary thread reply');
  await page.locator('#thread-form button[type="submit"]').click();
  await note(page,'#thread-log','An ordinary thread reply');
  assert.deepEqual(calls('channel.post').map(p=>[p.reply_to,p.body]),[[rootId,'An ordinary thread reply']]);
  assert.deepEqual(calls('decision.resolve'),answers);

  await page.locator('#thread-back').click();
  await page.locator('#home-back').click();
  const readOnly=page.locator('#channels button[data-channel="demo/lead+demo/worker"]');
  assert.equal(await readOnly.isVisible(),false,'read-only agent chats must start collapsed');
  await page.getByText('Behind the scenes · #demo',{exact:true}).tap();
  await readOnly.tap();await note(page,'#channel-log','The read-only example.');
  await dom(page,'#channel-readonly:not([hidden])','This conversation is read only.');
  assert.equal(await page.locator('#channel-form').isVisible(),false);
  await page.locator('#home-back').click();
  await page.locator('#channels button[data-channel="demo"]').click();
  await note(page,'#channel-log','Literal <b>sample</b>');
  assert.equal(await page.locator('#channel-log article b').count(),0,'body is text, not interpreted markup');
  const report=page.locator('#posts article',{hasText:'Literal <b>sample</b>'});
  const missing=choose(report,'summary.txt No longer there');
  assert.equal(await missing.isDisabled(),true,'the missing report stays visible but cannot be opened');
  assert.equal((await page.locator('body').innerText()).includes('/fixture/private'),false);
  assert.doesNotMatch(await page.locator('body').innerText(),/Woke |Who gets woken|Standup at/);
  // The compact rows need extra fixture text to keep this anchor reachable even in the tallest pane.
  const filler=server.fake.inject('demo',Array(20).fill('Fold and resize fixture line.').join('\n'));
  await dom(page,`#posts [data-post-id="${filler.id}"]`);
  const channelId=await page.locator('#posts article',{hasText:'Channel post 15.'}).getAttribute('data-post-id');
  const channelAnchor=await page.evaluate(id=>{
    const channel=document.querySelector('#channel-log');
    const row=channel.querySelector(`article[data-post-id="${id}"]`);
    channel.scrollTop+=row.getBoundingClientRect().top-channel.getBoundingClientRect().top+13;
    return row.getBoundingClientRect().top-channel.getBoundingClientRect().top;
  },channelId);
  await frame(page);
  // Open the original thread through its root, without focusing the log back to the bottom.
  await root.getByRole('button',{name:/^71 replies/}).evaluate(b=>b.focus({preventScroll:true}));
  await page.keyboard.press('Enter');
  await note(page,'#thread-log','An ordinary thread reply');
  await ta.fill('Draft survives folding');
  await ta.evaluate(e=>e.setSelectionRange(4,10));
  const replyId=await page.locator('#replies article',{hasText:'Thread reply 12.'}).getAttribute('data-post-id');
  const anchor=await page.evaluate(async id=>{
    await document.fonts.ready;
    const log=document.querySelector('#thread-log'), row=log.querySelector(`article[data-post-id="${id}"]`);
    log.scrollTop+=row.getBoundingClientRect().top-log.getBoundingClientRect().top+19;
    window.smokeDraft=document.querySelector('#thread-form textarea'); window.smokeLog=log;
    return row.getBoundingClientRect().top-log.getBoundingClientRect().top;
  },replyId);
  await frame(page);
  for(const [i,[width,height]] of widths.entries()) {
    await page.setViewportSize({width,height}); await frame(page);
    const view=await page.evaluate(([replyId,channelId])=>{
      const textarea=document.querySelector('#thread-form textarea'), log=document.querySelector('#thread-log');
      const offset=(container,id)=>container.querySelector(`[data-post-id="${id}"]`).getBoundingClientRect().top-container.getBoundingClientRect().top;
      const chan=document.querySelector('#channel-log');
      return {same:textarea===window.smokeDraft&&log===window.smokeLog,text:textarea.value,caret:[textarea.selectionStart,textarea.selectionEnd],
        panes:[...document.querySelectorAll('#app > .pane')].filter(e=>e.getBoundingClientRect().width>0).length,
        row:offset(log,replyId), channel:chan.getBoundingClientRect().height ? offset(chan,channelId) : null,
        bounds:[chan.scrollTop,chan.scrollHeight,chan.clientHeight],
        needs:document.querySelector('#needs').parentElement.id, overflow:document.documentElement.scrollWidth>innerWidth};
    },[replyId,channelId]);
    assert.equal(view.panes,[1,2,3,4,1][i]);
    assert.equal(view.needs,width>=2200?'needs-wide':'needs-home');
    assert.equal(view.same,true); assert.equal(view.text,'Draft survives folding'); assert.deepEqual(view.caret,[4,10]);
    assert.ok(Math.abs(view.row-anchor)<=2,`thread anchor changed at ${width}: ${anchor} → ${view.row}`);
    if(view.channel!==null) assert.ok(Math.abs(view.channel-channelAnchor)<=2,`channel anchor changed at ${width}: ${channelAnchor} → ${view.channel}; top/extent/view ${view.bounds}`);
    assert.equal(view.overflow,false);
    if(width>=1100) {
      await page.locator('#thread-back').focus();
      await page.keyboard.press('Tab');
      await page.keyboard.press('Shift+Tab');
      assert.equal(await page.locator('#thread-back').evaluate(e=>e===document.activeElement&&e.matches(':focus-visible')),true);
    }
    if(i<4 && process.env.TMPDIR) await page.screenshot({path:join(process.env.TMPDIR,`chat-${width}.png`)});
  }
  server.control.blocked=true; server.disconnect();
  await dom(page,'#app[data-phase="reconnecting"]');
  server.fake.inject('demo','A post committed while disconnected.');
  server.control.blocked=false; await page.evaluate(()=>dispatchEvent(new Event('online')));
  await current(page); await note(page,'#channel-log','A post committed while disconnected.');
  assert.equal(await page.locator('#channel-log article',{hasText:'A post committed while disconnected.'}).count(),1);
  assert.equal(await ta.inputValue(),'Draft survives folding');
  assert.equal(calls('channel.post').length,1);
  console.log('chat smoke: 412/840/1897/3840 CSS px, unlinked decisions, canonical thread replies, fold draft/caret/anchors, reconnect');
}

async function decisionActions() {
  for(const width of [412,1897]) {
    const touch=width===412, context=await browser.newContext({viewport:{width,height:915},hasTouch:touch});
    try {
      const page=await context.newPage();await online(context,page);
      const root=server.fake.inject('demo',`Linked overrule ${width}`,{kind:'question'});
      const id=server.fake.ask(`Linked overrule ${width}`,true,{channel:'demo',post_id:root.id});
      const pending=server.fake.ask(`Pending withdrawal ${width}`,false);
      await page.goto(server.origin);await current(page);
      const linked=findCard(page,id), outer=choose(linked,'Overrule: Second sample');
      const post=page.locator(`#posts [data-post-id="${root.id}"]`);
      await choose(linked,'Open conversation').click();await dom(page,`#posts [data-post-id="${root.id}"][data-focus=true]`);
      const at=calls('decision.resolve').length, posts=calls('channel.post').length;
      const over=choose(post,'Overrule: Second sample');
      if(touch) await over.tap(); else await over.press('Enter');
      assert.match(await modal(page).innerText(),/The work done on First sample is reopened and its owner is told\./);
      assert.equal(await choose(modal(page),'Cancel').evaluate(b=>b===document.activeElement),true);
      if(touch) await choose(modal(page),'Cancel').tap();else await page.keyboard.press('Escape');
      await closed(page);assert.equal(calls('decision.resolve').length,at);
      assert.equal(await page.locator('#decision-title').textContent(),'');
      assert.equal(await over.evaluate(b=>b===document.activeElement),true);
      await over.click();await choose(modal(page),'Confirm overrule').click();
      await dom(page,`#posts [data-post-id="${root.id}"]:not(:has(.opts))`);
      assert.deepEqual(calls('decision.resolve').slice(at),[{id,option:'Second sample'}]);
      assert.equal(calls('channel.post').length,posts);assert.equal(await outer.count(),0);
      if(touch) await page.locator('#home-back').tap();
      await choose(findCard(page,pending),'Withdraw').click();
      const reason=modal(page).getByLabel('Reason (optional)');
      await reason.fill('  Fixture is stale  ');
      const added=server.fake.ask(`Unrelated option listing ${width}`,false);
      await dom(page,`#cards [data-key="${added}"]`);assert.equal(await reason.inputValue(),'  Fixture is stale  ');
      let receipt;
      server.control.drop=(f,result)=>f.method==='decision.withdraw'&&f.params.id===pending&&!!(receipt=result);
      await choose(modal(page),'Confirm withdrawal').click();
      await dom(page,'#app[data-phase="reconnecting"]');
      assert.equal(receipt.decision.status,'withdrawn');delete server.control.drop;
      await page.evaluate(()=>dispatchEvent(new Event('online')));await current(page);
      assert.equal(await findCard(page,pending).count(),0);
      assert.deepEqual(calls('decision.withdraw').filter(p=>p.id===pending),[{id:pending,reason:'Fixture is stale'}]);
    } finally {delete server.control.drop;await context.close();}
  }
  console.log('decision actions: linked controls, cancel/focus, literal copy, withdrawal result lost and not replayed');
}

async function decisionBatch() {
  const context=await browser.newContext({viewport:{width:412,height:915},hasTouch:true});
  try {
    const [a,b,c]=['A','B','C'].map(letter=>server.fake.ask(`Bulk fixture ${letter}`,true));
    const pending=server.fake.ask('Pending control for bulk',false), page=await context.newPage();await online(context,page);
    const checked=[], held=Promise.withResolvers(), release=Promise.withResolvers();
    await page.routeWebSocket('**/ws',route=>{
      const ws=route.connectToServer();let first;
      route.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.method==='decision.resolve') {
          checked.push(f.params);
          if(f.params.id===a) first=f.id;
          if(f.params.id===b) {route.send(JSON.stringify({type:'response',id:f.id,error:{code:'refused',message:'Fixture refusal'}}));return;}
        }
        ws.send(raw);
      });
      ws.onMessage(raw=>{
        const f=JSON.parse(String(raw));
        if(f.id&&f.id===first&&f.result) {held.resolve();void release.promise.then(()=>route.send(raw));}
        else route.send(raw);
      });
    });
    await page.goto(server.origin);await current(page);
    const bulk=choose(page,"Keep all agents' picks (3)");
    assert.equal(await bulk.count(),1,'Needs you must offer the three eligible picks');
    await bulk.tap();assert.match(await modal(page).innerText(),/This confirms 3 provisional picks\./);
    await choose(modal(page),'Cancel').tap();await closed(page);assert.deepEqual(checked,[]);
    await bulk.tap();await choose(modal(page),'Keep all 3 picks').tap();await held.promise;
    await dom(page,'#needs','Keeping agents\' picks: 0 of 3 checked.');
    assert.equal(await choose(findCard(page,pending),'Withdraw').isDisabled(),true);
    assert.deepEqual(checked,[{id:a,option:'First sample'}]);release.resolve();
    await dom(page,'#needs','Kept 2 of 3 picks.');
    assert.deepEqual(checked,[a,b,c].map(id=>({id,option:'First sample'})));
    assert.equal(await findCard(page,a).count(),0);assert.equal(await findCard(page,c).count(),0);
    assert.match(await findCard(page,b).innerText(),/Couldn't confirm that answer\./);
    assert.equal(await findCard(page,pending).count(),1);
    assert.equal(await choose(page,/^Keep all agents' picks/).count(),0);
  } finally {await context.close();}
  console.log('decision batch: 3 serial writes, 2 kept, refusal still open, pending untouched');
}

const directId='demo/lead+owner';
const directRoot='We have <b>options</b>   for tomorrow.';
const directLast='Second direct reply.';
const directQuote='In reply to demo lead: We have <b>options</b> for tomorrow.';
async function openDirect(page) {
  await page.locator('#home-back').tap();
  await page.locator(`#channels [data-channel="${directId}"]`).tap();
  await dom(page,'#posts .body',directRoot);
  // On reload Live is the list/Watch/history barrier, not just a row arriving mid-refresh.
  await page.reload();await current(page);await home(page);
  await page.locator(`#channels [data-channel="${directId}"]`).tap();
}
async function workingSmoke(page) {
  server.fake.setActivity('demo/lead','idle'); // Opt this connection into the optional RPC.
  await page.setViewportSize({width:412,height:915});await page.goto(server.origin);await current(page);await home(page);
  await page.locator(`#channels [data-channel="${directId}"]`).tap();await dom(page,'#posts .body',directRoot);
  const selector='#channel-log + .activity[role="status"]', mark=page.locator(selector);
  assert.equal(await mark.count(),1,'the conversation has a stable activity live region');
  assert.equal(await mark.textContent(),'');
  server.fake.setActivity('demo/lead','working',true);
  await dom(page,selector,'demo lead is working…');
  assert.equal(await mark.isVisible(),true);await shot(page,'working-dm');
  server.fake.setActivity('demo/lead','idle');
  server.fake.inject(directId,'The turn is finished.');
  await dom(page,'#posts .body','The turn is finished.');await dom(page,`${selector}:empty`);
  assert.equal(await mark.evaluate(e=>e.getBoundingClientRect().height),0);
  assert.equal(await page.locator('#app').getAttribute('data-phase'),'live');
  server.fake.setActivity('demo/lead','attention_needed',true);
  await dom(page,selector,'demo lead needs you at the computer');
  console.log('Working smoke: delivery shows activity, the reply clears it without dropping the link, and attention has its own copy');
}

async function directChat(page) {
  await page.locator('#thread-back').tap();await openDirect(page);
  assert.deepEqual(await page.locator('#posts .body').allTextContents(),
    [directRoot,'First direct reply.','An ordinary direct message.',directLast]);
  assert.deepEqual(await page.locator('#posts .quote').allTextContents(),[directQuote,directQuote]);
  assert.equal(await page.locator('#posts blockquote b').count(),0);
  const root=page.locator('#posts article').filter({has:page.locator('.body',{hasText:directRoot})});
  const rootId=await root.getAttribute('data-post-id');
  const sheet=await messageActions(page,page.locator('#posts article',{hasText:directLast}));
  assert.equal(await choose(sheet,'Reply in thread').count(),0);await page.keyboard.press('Escape');
  await root.getByRole('button',{name:/^2 replies/}).tap();
  await note(page,'#thread-log',directLast);
  await page.locator('#thread-form textarea').fill('Separate DM thread draft');
  await page.locator('#thread-back').tap();
  assert.equal(await page.locator('#channel-form textarea').inputValue(),'');
  const project=page.locator('#channel-form textarea'), before=calls('decision.resolve').length;
  await project.fill('Talk, not a thread');await page.locator('#channel-form button[type=submit]').tap();
  await dom(page,'#posts .body','Talk, not a thread');
  const sent=calls('channel.post').at(-1);
  assert.equal(sent.channel,directId);assert.equal(sent.reply_to,undefined);assert.ok(sent.client_key);
  assert.equal(calls('decision.resolve').length,before);
  await root.getByRole('button',{name:/^2 replies/}).tap();
  assert.equal(await page.locator('#thread-form textarea').inputValue(),'Separate DM thread draft');
  assert.equal(await page.locator('#replies article').first().getAttribute('data-post-id'),rootId);
  await page.locator('#thread-back').tap();await page.locator('#home-back').tap();
  const linkedChannel='demo/chief+owner';
  const question=server.fake.inject(linkedChannel,'Older linked question',{kind:'question'});
  for(let i=0;i<51;i++) server.fake.inject(linkedChannel,`Direct buffer ${i}`);
  const id=server.fake.ask('A linked decision',false,{channel:linkedChannel,post_id:question.id});
  const card=page.locator(`#cards [data-key="${id}"]`), at=calls('channel.history').length;
  await card.getByRole('button',{name:'Open conversation'}).tap();
  const target=`#posts [data-post-id="${question.id}"]`;
  await dom(page,`${target}[data-focus=true]`);
  assert.equal(await page.locator(target).evaluate(e=>e===document.activeElement),true);
  assert.equal(await page.locator('#app').getAttribute('data-pane'),'channel');
  assert.ok(calls('channel.history').slice(at).some(p=>p.channel===linkedChannel&&p.before),'linked decision paged back');
}

async function sidebar(page) {
  const {second,cross,seatSystem,target}=server.fake.sidebar();
  await page.reload();await current(page);await home(page);
  const nav=page.locator('#channels'), item=id=>nav.locator(`[data-channel="${id}"]`);
  assert.deepEqual(await nav.locator('summary').allTextContents(),['Behind the scenes · #beta','Behind the scenes · #demo']);
  for(const id of ['demo','beta','demo/lead+owner','owner+system']) assert.equal(await item(id).isVisible(),true);
  for(const id of [second,cross]) assert.equal(await item(id).isVisible(),false);
  assert.equal(await item(seatSystem).count(),0);
  await page.getByText('Behind the scenes · #demo',{exact:true}).tap();
  assert.equal(await item(second).isVisible(),true);assert.equal(await item(cross).isVisible(),false);
  assert.equal(await item(second).locator('.badge').textContent(),'1');
  server.fake.inject(second,'Another read-only message',{author:'demo/a'});
  await dom(page,`#channels [data-channel="${second}"] .badge`,'2');
  assert.equal(await item(second).isVisible(),true,'live updates must not close the fold');
  await item(second).tap();await dom(page,'#posts .body','Another read-only message');
  assert.equal(await page.locator('#channel-readonly').isVisible(),true);
  await page.locator('#home-back').tap();
  const id=server.fake.ask('Linked hidden conversation',false,{channel:seatSystem,post_id:target.id});
  await page.locator(`#cards [data-key="${id}"]`).getByRole('button',{name:'Open conversation'}).tap();
  await dom(page,`#posts [data-post-id="${target.id}"][data-focus=true]`);
  assert.equal(await page.locator('#channel-readonly').isVisible(),true);
  await page.setViewportSize({width:1897,height:1100});await frame(page);
  const crossFold=page.getByText('Behind the scenes · #beta',{exact:true});
  await crossFold.press('Space');
  assert.equal(await item(cross).isVisible(),true);
  assert.equal(await item(cross).count(),1,'one cross-project row, attributed once');
}

function fixturePost(page,id) { return page.locator(`#posts article[data-post-id="${id}"]`); }
async function editSample(page) {
  const {id}=server.fake.inject('demo','Edit smoke original',{author:'owner'});
  const row=fixturePost(page,id);
  await dom(page,`#posts [data-post-id="${id}"]`);
  await choose(await messageActions(page,row),'Edit').click();await row.getByRole('textbox',{name:'Edit message'}).fill('Edit smoke saved');
  await choose(row,'Save').click();await dom(page,`#posts [data-post-id="${id}"] .post-status`,'edited');
  assert.deepEqual(calls('channel.edit').at(-1),{post:id,body:'Edit smoke saved'});
  assert.equal(await row.locator('.body').textContent(),'Edit smoke saved');
  return id;
}
async function postMutationSmoke(page) {
  await page.goto(server.origin);await current(page);
  await page.locator('#channels button[data-channel="demo"]').click();
  const id=await editSample(page), row=fixturePost(page,id);
  await choose(await messageActions(page,row),'Thumbs up').tap();
  assert.equal(await actionSheet(page).count(),0);
  await dom(page,`#posts [data-post-id="${id}"] button[aria-pressed=true]:not([disabled])`,'👍 1');
  assert.deepEqual(calls('channel.react').at(-1),{post:id,emoji:'👍'});
  await choose(await messageActions(page,row),'Remove').click();await choose(modal(page),'Remove message').click();
  await dom(page,`#posts [data-post-id="${id}"] .body`,'removed');
  assert.deepEqual(calls('channel.remove').at(-1),{post:id});
  console.log('post mutations: inline edit, reaction toggle and confirmed removal through fake RPC');
}
async function pwa(context,page) {
  const controlWorker=()=>page.evaluate(async()=>{
    await navigator.serviceWorker.ready;
    if(!navigator.serviceWorker.controller) await new Promise(r=>navigator.serviceWorker.addEventListener('controllerchange',r,{once:true}));
  });
  await page.goto(server.origin);
  assert.equal(await page.locator('link[rel="manifest"]').count(),1,'install manifest is missing');
  const policy=await page.evaluate(()=>document.querySelector('meta[http-equiv="Content-Security-Policy"]')?.content??'');
  assert.match(policy,/(?:^|; )default-src 'self'(?:;|$)/);
  assert.match(policy,/(?:^|; )connect-src 'self'(?:;|$)/);
  await current(page);
  const fontWeights=await page.evaluate(async()=>{
    await document.fonts.ready;
    return [...document.fonts].filter(f=>f.family==='Atkinson'&&f.status==='loaded').map(f=>f.weight).sort();
  });
  assert.deepEqual(fontWeights,['400','700']);
  const manifest=await page.evaluate(async()=>{
    const response=await fetch(document.querySelector('link[rel="manifest"]').href);
    return response.json();
  });
  assert.equal(manifest.display,'standalone');assert.equal(manifest.start_url,'/');assert.equal(manifest.scope,'/');
  assert.deepEqual(manifest.icons.map(i=>i.sizes),['192x192','512x512']);
  for(const [i,sz] of [192,512].entries()) {
    const png=await (await context.request.get(`${server.origin}${manifest.icons[i].src}`)).body();
    assert.equal(png.readUInt32BE(16),sz);assert.equal(png.readUInt32BE(20),sz);
  }
  await controlWorker();
  await page.locator('#channels button[data-channel="demo"]').click();
  await note(page,'#channel-log','Literal <b>sample</b>');
  const edited=await editSample(page);
  await context.setOffline(true);server.disconnect();
  await dom(page,'#app[data-phase="offline"]');await page.reload();
  await dom(page,'#connection-banner:not([hidden])',"You're offline");await home(page);
  await page.locator('#channels button[data-channel="demo"]').tap();
  await note(page,'#channel-log','Literal <b>sample</b>');
  assert.equal(await fixturePost(page,edited).locator('.post-status').textContent(),'edited');
  assert.equal(await choose(fixturePost(page,edited),'Edit').count(),0);
  assert.equal(await composerButton(page).isDisabled(),true);
  assert.equal(await page.locator('#channel-log article .body').first().evaluate(e=>getComputedStyle(e).color),'rgb(182, 177, 189)');
  const cached=await page.evaluate(async()=>Promise.all((await caches.keys()).map(async k=>(await (await caches.open(k)).keys()).map(x=>new URL(x.url).pathname))));
  assert.ok(cached.flat().includes('/index.html'));
  assert.ok(cached.flat().every(path=>path!=='/ws'&&path!=='/health'));
  await page.evaluate(()=>localStorage.clear());await page.reload();
  await dom(page,'#app[data-phase="offline"]');assert.equal(await page.locator('#channel-log article').count(),0);
  await context.setOffline(false);
  server.fake.inject('demo','A fresh canonical post after offline reload.');
  await current(page);await note(page,'#channel-log','A fresh canonical post after offline reload.');
  await page.locator('#channels button[data-channel="demo"]').click();
  await page.locator('#channel-form textarea').fill('Keep this draft on reload');
  const update=()=>page.evaluate(async()=>{
    const registration=await navigator.serviceWorker.getRegistration();
    const outcome=new Promise(resolve=>{
      registration.addEventListener('updatefound',()=>{
        const worker=registration.installing;
        const check=()=>{if(['installed','redundant'].includes(worker.state)) resolve(worker.state);};
        worker.addEventListener('statechange',check);check();
      },{once:true});
    });
    await registration.update();return outcome;
  });
  const revision=mkdtempSync(join(tmpdir(),'atm-shell-'));
  try {
    cpSync(dir,revision,{recursive:true});appendFileSync(join(revision,'index.html'),'\n<!-- second shell -->\n');
    execFileSync(process.execPath,['scripts/build-shell.mjs',revision]);
    server.publish(revision);server.control.omit='/icon-512.png';
    assert.equal(await update(),'redundant');
    assert.equal(await page.locator('#update-banner').isVisible(),false);
    assert.equal(await page.locator('#channel-form textarea').inputValue(),'Keep this draft on reload');
    server.control.omit=null;
    assert.equal(await update(),'installed');await dom(page,'#update-banner:not([hidden])','Update ready');
    const navigation=page.waitForEvent('framenavigated');
    await page.locator('#update-reload').click();await navigation;await current(page);await controlWorker();
    assert.equal(await page.locator('#channel-form textarea').inputValue(),'Keep this draft on reload');
    assert.equal(await page.locator('#update-banner').isVisible(),false);
    assert.equal(await pane(page),'channel','explicit worker Reload resumes the conversation');
    await page.goBack();await home(page);await page.goForward();
    assert.equal(await page.locator('#channel-form textarea').inputValue(),'Keep this draft on reload');
    assert.equal(await page.evaluate(async()=>(await caches.keys()).filter(k=>k.startsWith('atm-chat-shell-')).length),1);
  } finally { rmSync(revision,{recursive:true,force:true}); }
  await openDirect(page);
  assert.deepEqual(await page.locator('#posts .quote').allTextContents(),[directQuote,directQuote]);
  await context.setOffline(true);server.disconnect();
  await dom(page,'#app[data-phase="offline"]');await page.reload();
  await dom(page,'#app[data-phase="offline"]');await home(page);
  await page.locator(`#channels [data-channel="${directId}"]`).tap();
  assert.deepEqual(await page.locator('#posts .quote').allTextContents(),[directQuote,directQuote]);
  assert.equal(await composerButton(page).isDisabled(),true);
  await context.setOffline(false);await page.evaluate(()=>dispatchEvent(new Event('online')));await current(page);
  await page.locator('#home-back').tap();
  const id=server.fake.ask('Identity purge fixture?',false);
  await dom(page,`#cards [data-key="${id}"]`);
  await choose(findCard(page,id),'Withdraw').tap();
  await modal(page).getByLabel('Reason (optional)').fill('private fixture reason');
  server.control.health='identity';server.control.refusal='identity';server.disconnect();
  await dom(page,'#app[data-phase="identity"]');
  assert.equal(await modal(page).count(),0);
  assert.equal(await page.locator('#decision-input').inputValue(),'');
  assert.equal(await findCard(page,id).count(),0);
  assert.equal(calls('decision.withdraw').some(p=>p.id===id),false);
  assert.equal(await page.locator('#posts .quote').count(),0);
  assert.equal(await page.locator('#channel-log article').count(),0);
  assert.equal(await page.evaluate(()=>localStorage.length),0);
  console.log('PWA smoke: manifest/icons, offline reload with and without snapshot, recovery, failed precache, waiting update and draft reload, identity purge');
}

// Chromium considers a net namespace with only loopback offline by default.
async function online(context,page) {
  if(!await page.evaluate(()=>navigator.onLine)) {
    const cdp=await context.newCDPSession(page);
    await cdp.send('Network.overrideNetworkState',{offline:false,latency:0,downloadThroughput:-1,uploadThroughput:-1});
  }
}

try {
  browser=await chromium.launch({args:['--disable-background-networking','--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1']});
  const context=await browser.newContext({viewport:{width:412,height:915},hasTouch:true,deviceScaleFactor:1});
  context.on('request',req=>{if(!req.url().startsWith(server.origin+'/')) external.push(req.url());});
  await context.exposeBinding('recordCspViolation',(_source,details)=>policyViolations.push(details));
  await context.addInitScript(()=>{
    document.addEventListener('securitypolicyviolation',e=>window.recordCspViolation(`${e.violatedDirective}: ${e.blockedURI}`));
  });
  const page=await context.newPage();
  await online(context,page);
  page.on('pageerror',error=>console.error('Browser page error:',error.message));
  if(process.argv.includes('--pwa')) await pwa(context,page);
  else if(process.argv.includes('--history')) await historySmoke();
  else if(process.argv.includes('--pull')) await pullSmoke();
  else if(process.argv.includes('--composer')) await composerSmoke();
  else if(process.argv.includes('--composer-state')) await composerStateSmoke();
  else if(process.argv.includes('--composer-keys')) await composerKeys();
  else if(process.argv.includes('--working')) await workingSmoke(page);
  else {await chat(page);await fineThreadAction();await directChat(page);await sidebar(page);await decisionActions();await decisionBatch();await historySmoke();await pullSmoke();await composerSmoke();await composerStateSmoke();await composerKeys();await workingSmoke(page);await postMutationSmoke(page);}
  assert.deepEqual(external,[]);
  assert.deepEqual(policyViolations,[]);
  console.log(`Chromium ${browser.version()} loopback only`);
} finally { await browser?.close(); await server.close(); }
}

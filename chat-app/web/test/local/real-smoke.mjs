import assert from 'node:assert/strict';
import {mkdirSync} from 'node:fs';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
import {journal,realRig,topics} from './real-rig.mjs';
import {touchDrag} from './touch.mjs';
import {photosBrowser} from './photos.browser.mjs';

const viewport={width:412,height:915};
const pending='Which real option should we select?', provisional='Confirm the provisional option?', tag='Real <b>literal</b> with @owner';
const channel='demo';
const draft=(page,kind='channel')=>page.locator(`#${kind}-form textarea`);
const send=(page,kind='channel')=>page.locator(`#${kind}-form button[type=submit]`);
const post=(page,id,box='posts')=>page.locator(`#${box} article[data-post-id="${id}"]`);
const card=(page,id)=>page.locator(`#cards article[data-key="${id}"]`);
function dom(page,selector,text='') {
  return page.evaluate(([sel,copy])=>new Promise(resolve=>{
    const observer=new MutationObserver(check);
    function check(){if([...document.querySelectorAll(sel)].some(e=>e.textContent.includes(copy))){observer.disconnect();resolve();}}
    observer.observe(document,{subtree:true,attributes:true,childList:true,characterData:true});check();
  }),[selector,text]);
}
const phase=(page,name)=>dom(page,`#app[data-phase="${name}"]`);
const shown=(page,id,box='posts')=>dom(page,`#${box} [data-post-id="${id}"]`);
const alive=page=>phase(page,'live');
async function openChat(page,origin) {
  const entry=await page.goto(origin,{waitUntil:'domcontentloaded'});
  assert.equal(await page.evaluate(()=>navigator.onLine),true,'Chromium reports offline in the sealed network namespace');
  await alive(page);
  return entry;
}
const choice=(page,name)=>page.getByRole('button',{name,exact:true});
const dialog=page=>page.getByRole('dialog');
async function messageActions(page,row) {
  await choice(row,'Message actions').press('Enter');return page.getByRole('dialog',{name:'Message actions'});
}
const removed=(page,id)=>page.evaluate(id=>new Promise(resolve=>{
  const observer=new MutationObserver(check);
  function check(){if(!document.querySelector(`#cards [data-key="${id}"]`)){observer.disconnect();resolve();}}
  observer.observe(document,{childList:true,subtree:true});check();
}),id);

// All normal frames traverse Chromium's native connection to the real bridge.
// Fault cases only withhold a particular frame or end the current test link.
async function traceSocket(page) {
  const frames=journal();let link=0, prior=0, drop, withoutProof, hold, pause, idleNext=false, current;
  await page.routeWebSocket('**/ws',route=>{
    const id=++link, server=route.connectToServer(), idle=idleNext;
    idleNext=false;let droppedId, ended=false;
    route.onMessage(raw=>{
      const f=JSON.parse(String(raw));frames.add({id,f,dir:'send'});
      if(f.type==='watch')prior=f.cursor;
      if(f.type==='connect') {
        if(idle)return;
        if(hold){void hold.promise.then(()=>server.send(raw));return;}
      }
      if(f.method==='channel.post'&&f.params.body===drop) {droppedId=f.id;withoutProof=f.params.client_key;}
      if(pause&&!pause.request&&f.method==='channel.history'&&f.params.channel===channel) pause.request=f;
      server.send(raw);
    });
    server.onMessage(raw=>{
      if(ended)return;
      const f=JSON.parse(String(raw));
      if(f.id&&f.id===droppedId&&f.result){
        const cursor=prior;drop=undefined;ended=true;cut(false);frames.add({id,f,dir:'dropped',cursor});return;
      }
      // The replay case loses both exact proofs, not just the receipt.
      if(withoutProof&&f.kind==='channel.posted'&&f.body?.client_key===withoutProof) return;
      if(f.type==='event')prior=Math.max(prior,f.seq);
      frames.add({id,f,dir:'recv'});
      if(pause?.request?.id===f.id&&f.type==='response') {
        const held=pause;pause=undefined;held.arrived.resolve(f);void held.release.promise.then(()=>route.send(raw));return;
      }
      route.send(raw);
    });
    server.onClose((code,reason)=>{
      frames.add({id,dir:'close',code,reason,idle});
      void route.close({code:code===1006||code===1005?undefined:code,reason});
    });
    function cut(idleCut) {
      ended=true;idleNext=idleCut;
      if(!idleCut)hold=Promise.withResolvers();
      void route.close({code:1011,reason:'fixture disconnect'});
      void server.close({code:1000});
    }
    current={cut};
  });
  return {frames,dropPost(body){drop=body;},cut(idle=false){current.cut(idle);},release(){const ready=hold;hold=undefined;ready.resolve();},
    holdHistory(){const next={arrived:Promise.withResolvers(),release:Promise.withResolvers()};pause=next;return next;},
    sent(method,from=0){return frames.rows.slice(from).filter(r=>r.dir==='send'&&r.f.method===method).map(r=>r.f);},
    reply(request,from=0){return frames.next(r=>r.dir==='recv'&&r.f.type==='response'&&r.f.id===request.id,from);}};
}

async function seed(rig,wire) {
  const {seq}=await wire.rpc('channel.list');wire.watch(seq);
  const dm=(seat,to,body)=>rig.json(seat,'post','--to',to,'--body',body);
  dm('chief','owner','Chief fixture direct');
  const {channel:direct,post_id:directRoot}=dm('lead','owner','Lead fixture direct');
  const firstReply=(await wire.rpc('channel.post',{reply_to:directRoot,body:'First real direct reply'})).post_id;
  const nextRoot=(await wire.rpc('channel.post',{channel:direct,body:'Interleaved real direct root'})).post_id;
  const lastReply=(await wire.rpc('channel.post',{reply_to:directRoot,body:'Second real direct reply'})).post_id;
  const roots=await wire.rpc('channel.history',{channel:direct});
  const firstThread=await wire.rpc('channel.history',{channel:direct,thread:directRoot,limit:2});
  const nextThread=await wire.rpc('channel.history',{channel:direct,thread:directRoot,after:firstReply,limit:2});
  assert.deepEqual(roots.posts.map(p=>p.id),[nextRoot,directRoot]);
  assert.deepEqual(firstThread.posts.map(p=>p.id),[directRoot,firstReply]);assert.equal(firstThread.more,true);
  assert.deepEqual(nextThread.posts.map(p=>p.id),[lastReply]);assert.equal(nextThread.posts[0].thread,directRoot);
  assert.equal(nextThread.more,false);
  console.log('real direct history:',JSON.stringify({roots:roots.posts.map(p=>p.id),first:firstThread.posts.map(p=>p.id),after:firstReply,next:nextThread.posts.map(p=>p.id)}));
  const ro=dm('worker','demo/lead','Read-only fixture direct').channel;
  rig.json(null,'agent','down','demo/chief');
  const wake=dm(null,'demo/chief','A notice for a down chief').woke[0].message_id;
  const notice=await wire.frames.next(f=>f.kind==='msg.sent'&&f.body.to==='owner'&&f.body.delivery_notice?.source_message_id===wake);
  const root=(await wire.rpc('channel.post',{channel,body:'An older real thread root'})).post_id;
  const replies=[];
  for(let i=0;i<51;i++) replies.push((await wire.rpc('channel.post',{reply_to:root,body:`Seed reply ${i}`})).post_id);
  for(let i=0;i<51;i++) await wire.rpc('channel.post',{channel,body:`Seed root ${i}`});
  const ask=(title,...extra)=>rig.json('worker','decisions','request','--kind','plan','--title',title,
    '--option','Keep this','--option','Try that','--may-be-undone','true','--changes-intent','true',...extra).decision_id;
  rig.cli(null,'away');const p=ask(provisional,'--recommended','Keep this');
  await wire.frames.next(f=>f.kind==='decision.taken_provisionally'&&f.body.decision_id===p);
  rig.cli(null,'back');const d=ask(pending);
  const first=await wire.rpc('decision.list',{limit:1});assert.equal(first.next_after,p);
  const second=await wire.rpc('decision.list',{limit:1,after:p});assert.equal(second.next_after,null);
  assert.equal(second.decisions[0].id,d);
  const questions=first.decisions.concat(second.decisions);
  for(const q of questions) if(q.post_id||q.channel) {
    assert.equal(q.channel,channel);assert.match(q.post_id,/^m\d+$/);
    const asked=await wire.frames.next(f=>f.kind==='channel.posted'&&f.body.decision?.decision_id===q.id&&f.body.decision.step==='asked');
    assert.equal(`m${asked.seq}`,q.post_id);
  }
  console.log('real decision projection:',JSON.stringify(questions.map(({id,post_id,channel})=>({id,post_id,channel}))));
  const tagged=rig.json('worker','post','--channel',channel,'--body',tag,'--attach','fixture/summary.txt');
  assert.deepEqual(tagged.mentions,['owner']);
  const {channels}=await wire.rpc('channel.list'), unread=channels.find(c=>c.id===channel).unread;
  assert.ok(unread>0);
  return {ro,system:`m${notice.seq}`,root,replies,tagged:tagged.post_id,d,p,ask,questions,unread,
    direct,directRoot,directOrder:[directRoot,firstReply,nextRoot,lastReply]};
}

async function assetChecks(page,entry) {
  const headers=await entry.allHeaders();
  assert.equal(entry.status(),200);assert.match(headers['content-type'],/^text\/html/);
  assert.equal(headers['cache-control'],'no-store');assert.equal(headers['x-content-type-options'],'nosniff');
  const assets=await page.evaluate(async()=>{
    await document.fonts.ready;
    // ready fires only when the real bridge's worker has installed its static shell.
    const {active}=await navigator.serviceWorker.ready;
    const refs=[document.querySelector('script[type=module]').src,document.querySelector('link[rel=stylesheet]').href,
      performance.getEntriesByType('resource').find(r=>r.name.endsWith('.woff2')).name,
      document.querySelector('link[rel=manifest]').href,active.scriptURL];
    return Promise.all(refs.map(async url=>{
      const r=await fetch(url,{cache:'no-store'});
      return [r.status,r.headers.get('content-type'),r.headers.get('cache-control'),r.headers.get('x-content-type-options')];
    }));
  });
  const types=[/^text\/javascript/,/^text\/css/,/^font\/woff2/,/^application\/json/,/^text\/javascript/];
  for(const [i,[status,mime,cache,guard]] of assets.entries()) {
    const type=types[i];
    assert.equal(status,200);assert.match(mime,type);assert.equal(cache,'no-store');assert.equal(guard,'nosniff');
  }
  console.log('real static: HTML/JS/CSS/WOFF2/manifest/worker headers; worker active');
}

async function screenshot(page,width,height) {
  await page.setViewportSize({width,height});
  const layout=await page.evaluate(async()=>{
    await document.fonts.ready;await new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)));
    return [...document.querySelectorAll('#app > .pane')].filter(el=>el.getBoundingClientRect().width).length;
  });
  assert.equal(layout,{412:1,840:2,1897:3}[width]);
  const dir=process.env.ATM_CHAT_TEST_SHOTS;
  if(dir){mkdirSync(dir,{recursive:true});await page.screenshot({path:join(dir,`real-${width}.png`)});}
}

async function realGestures(page,cdp,trace,tagged) {
  const pane=()=>page.locator('#app').getAttribute('data-pane');
  const focus=async(move=false)=>page.evaluate(async move=>{
    const log=document.getElementById('channel-log'), row=log.querySelectorAll('article').item(12);
    await document.fonts.ready;
    const offset=()=>row.getBoundingClientRect().top-log.getBoundingClientRect().top;
    if(move) log.scrollTop+=offset()+15;
    return [row.dataset.postId,offset()];
  },move);
  // Set the anchor while the channel and the thread are both visible.
  await page.setViewportSize({width:1897,height:1100});
  await draft(page).fill('Real draft across Back and pull');
  await draft(page).evaluate(el=>el.setSelectionRange(2,9));
  await focus(true);
  await page.setViewportSize(viewport);await page.goBack();
  assert.equal(await pane(),'channel');
  const anchor=await focus(true);
  await page.goForward();assert.equal(await pane(),'thread');
  await page.goBack();assert.equal(await pane(),'channel');
  const [id,offset]=await focus();assert.equal(id,anchor[0]);assert.ok(Math.abs(offset-anchor[1])<=2);
  assert.equal(await draft(page).inputValue(),'Real draft across Back and pull');
  assert.deepEqual(await draft(page).evaluate(el=>[el.selectionStart,el.selectionEnd]),[2,9]);
  const dir=process.env.ATM_CHAT_TEST_SHOTS;
  if(dir){mkdirSync(dir,{recursive:true});await page.screenshot({path:join(dir,'real-412-back.png')});}
  await page.goForward();await page.setViewportSize({width:1897,height:1100});
  assert.equal(await page.locator('#channel-log').isVisible(),true);
  await page.goBack();assert.equal(await pane(),'channel');
  assert.equal(await page.locator('#app').getAttribute('data-thread'),'false');
  const log=page.locator('#channel-log');
  await log.evaluate(el=>{
    const width=el.clientWidth;
    window.channelResized=new Promise(resolve=>{
      const observer=new ResizeObserver(()=>{if(el.clientWidth!==width){observer.disconnect();resolve();}});
      observer.observe(el);
    });
  });
  await page.setViewportSize(viewport);
  // Layout first: the app's resize observer restores the old anchor at the new width.
  await page.evaluate(()=>window.channelResized);
  await log.evaluate(async el=>{
    if(!el.scrollTop) return;
    const atTop=new Promise(resolve=>el.addEventListener('scroll',resolve,{once:true}));
    el.scrollTop=0;await atTop;
  });
  assert.equal(await log.evaluate(el=>el.scrollTop),0,'a real pull starts at the top after resize');
  const {x,y,width}=await log.boundingBox(), mark=trace.frames.rows.length, url=page.url();
  let documents=0;const record=req=>{if(req.resourceType()==='document') documents++;};page.on('request',record);
  const held=trace.holdHistory();
  await touchDrag(cdp,x+width/2,y+70,0,104);
  await dom(page,'#connection-label','Refreshing…');
  const result=await held.arrived.promise;
  assert.ok(result.result);assert.equal(result.id,held.request.id);assert.equal(result.result.channel,channel);
  const list=trace.sent('channel.list',mark).at(0);assert.ok(list);assert.ok((await trace.reply(list,mark)).f.result);
  held.release.resolve();await dom(page,'#connection-label','Live');
  assert.equal(await draft(page).inputValue(),'Real draft across Back and pull');
  assert.equal(await post(page,tagged).count(),1);
  assert.equal(page.url(),url);assert.equal(documents,0);page.off('request',record);
  for(const method of ['channel.post','decision.resolve','decision.withdraw']) assert.deepEqual(trace.sent(method,mark),[]);
  await page.goForward();assert.equal(await pane(),'thread');
  console.log('real gestures: native thread-first Back at 412 and 1897, draft/caret and anchor, touch pull over private bridge');
}

async function chat(page,rig,wire,trace,s,cdp) {
  const {root,replies,tagged,d,p,ro,system,direct,directRoot,directOrder}=s;
  const entry=await openChat(page,rig.origin);
  assert.deepEqual(await page.locator('#channels h2').allTextContents(),['Chief of staff','Leads','Channels','Direct messages']);
  assert.equal(trace.sent('channel.seen').length,0,'home has no visible log');
  assert.equal(await card(page,d).count(),1,'real pending decision must be in Needs you');
  for(const q of s.questions) assert.equal(await card(page,q.id).locator('.open').count(),q.post_id?1:0);
  assert.deepEqual(await card(page,p).locator('button:not(.open)').allTextContents(),['Keep Keep this','Overrule: Try that']);
  assert.equal(await page.locator('#channels [data-channel="demo"] .badge').textContent(),String(s.unread));
  await assetChecks(page,entry);await screenshot(page,412,915);
  const select=id=>page.locator(`#channels [data-channel="${id}"]`).click();
  assert.equal(await page.locator(`#channels [data-channel="${ro}"]`).isVisible(),false);
  await page.getByText('Behind the scenes · #demo',{exact:true}).tap();
  await select(ro);await dom(page,'#posts .body','Read-only fixture direct');
  await dom(page,'#channel-readonly:not([hidden])');assert.equal(await draft(page).isVisible(),false);
  await page.locator('#home-back').click();
  await select('owner+system');await shown(page,system);
  await page.locator('#home-back').click();
  const at=trace.frames.rows.length;
  await select(direct);
  await shown(page,directRoot);
  const readDirect=await trace.frames.next(r=>r.dir==='send'&&r.f.method==='channel.seen'&&r.f.params.channel===direct,at);
  await trace.reply(readDirect.f,at);
  assert.deepEqual(await page.locator('#posts article').evaluateAll(rows=>rows.map(r=>r.dataset.postId)),directOrder,'real direct replies must be inline by ID');
  assert.equal(readDirect.f.params.through,directOrder.at(-1));
  assert.deepEqual(await page.locator('#posts .quote').allTextContents(),Array(2).fill('In reply to demo lead: Lead fixture direct'));
  assert.equal(await page.locator('#thread-form').isVisible(),false);
  const replyOnly=(await wire.rpc('channel.post',{reply_to:directRoot,body:'A real reply-only update'})).post_id;
  await shown(page,replyOnly);
  const directBody='A normal direct conversation post', directAt=trace.frames.rows.length;
  await draft(page).fill(directBody);await draft(page).press('Enter');
  assert.equal(await draft(page).inputValue(),`${directBody}\n`);
  assert.equal(trace.sent('channel.post',directAt).length,0,'touch Return does not submit');
  const directNote=await submit(page,trace,`${directBody}\n`);
  assert.equal(directNote.request.params.channel,direct);assert.equal(directNote.request.params.reply_to,undefined);
  assert.equal(directNote.result.thread,null);await shown(page,directNote.result.post_id);
  const ownerPost=post(page,directNote.result.post_id);
  const ownerActions=await messageActions(page,ownerPost);
  for(const [method,label] of [['channel.edit','Edit'],['channel.remove','Remove'],['channel.react','Thumbs up']]) if(!wire.connected.methods.includes(method)) {
    assert.equal(await choice(ownerActions,label).count(),0,`old daemon must not expose ${label}`);
    assert.equal(trace.sent(method).length,0);
  }
  await page.keyboard.press('Escape');
  await page.locator('#home-back').click();
  assert.equal(trace.sent('channel.seen').some(f=>f.params.channel===channel),false,'hidden project log is not read');
  await select(channel);await shown(page,tagged);
  assert.deepEqual(await post(page,tagged).locator('.mention').allTextContents(),['@owner']);
  assert.equal(await post(page,tagged).locator('.files').textContent(),'summary.txt');
  assert.equal(await post(page,tagged).locator('.body').textContent(),tag);
  assert.equal(await post(page,tagged).locator('b').count(),0);
  const read=await trace.frames.next(r=>r.dir==='send'&&r.f.method==='channel.seen'&&r.f.params.channel===channel&&r.f.params.through===tagged);
  assert.equal((await trace.reply(read.f)).f.result.through,tagged);
  const list=await wire.rpc('channel.list');assert.equal(list.channels.find(c=>c.id===channel).seen_through,tagged);
  assert.equal(list.channels.find(c=>c.id===channel).unread,0);
  const older=await wire.rpc('channel.history',{channel,limit:50});assert.ok(older.more);
  const start=trace.frames.rows.length;
  await page.locator('#older').click();await shown(page,root);
  assert.ok(trace.sent('channel.history',start).some(f=>f.params.before===older.posts.at(-1).id));
  const first=await wire.rpc('channel.history',{channel,thread:root,limit:50});
  assert.equal(first.more,true);assert.equal(first.posts[0].id,root);
  assert.deepEqual(first.posts.slice(1).map(x=>x.id),replies.slice(0,49));
  const after=await wire.rpc('channel.history',{channel,thread:root,after:first.posts.at(-1).id});
  assert.deepEqual(after.posts.map(x=>x.id),replies.slice(49));assert.equal(after.more,false);
  await post(page,root).getByRole('button',{name:/^51 replies/}).tap();
  await shown(page,replies.at(-1),'replies');assert.equal(await post(page,root,'replies').count(),1);
  await realGestures(page,cdp,trace,tagged);
  const reply=await submit(page,trace,'A browser thread reply','thread');
  assert.equal(reply.request.params.reply_to,root);assert.equal(reply.result.thread,root);
  await shown(page,reply.result.post_id,'replies');
  assert.ok((await wire.rpc('decision.list')).decisions.some(row=>row.id===d),'ordinary reply must not resolve a decision');
  console.log('real chat: ordinary thread reply committed without resolving a decision');
  await screenshot(page,840,900);await screenshot(page,1897,1100);
  await page.setViewportSize(viewport);await page.locator('#thread-back').click();
  const note=await submit(page,trace,'A browser top-level post');
  assert.equal(note.request.params.channel,channel);assert.equal(note.result.thread,null);await shown(page,note.result.post_id);
  await page.locator('#home-back').click();await dom(page,'#app[data-pane=home]');
  console.log('real chat: ordinary top-level post committed, returned Home');
  const linked=s.questions.find(q=>q.id===d)?.post_id;
  if(linked) {
    await choice(card(page,d),'Open conversation').tap();
    await dom(page,`#posts [data-post-id="${linked}"][data-focus=true]`);
    assert.equal(await post(page,linked).locator('.tag').textContent(),'Question');
    await page.goBack();await dom(page,'#app[data-pane=home]');
    console.log('real chat: native Back from linked conversation',linked);
  }
  for(const [id,option,button] of [[d,'Try that','Try that'],[p,'Keep this','Keep Keep this']]) {
    await choice(card(page,id),button).click();
    const resolved=await wire.frames.next(f=>f.kind==='decision.resolved'&&f.body.decision_id===id);
    assert.equal(resolved.body.option,option); await removed(page,id);
    if(s.questions.some(q=>q.id===id&&q.post_id)) {
      // A U8 reply follows the receipt; its Watch event invalidates any preflight
      // already in flight. Wait for the browser's subsequent list before the next answer.
      const step=await trace.frames.next(r=>r.dir==='recv'&&r.f.kind==='channel.posted'&&
        r.f.body.decision?.decision_id===id&&r.f.body.decision.step==='resolved');
      const after=trace.frames.rows.indexOf(step)+1;
      const listed=await trace.frames.next(r=>r.dir==='send'&&r.f.method==='decision.list',after);
      const {f}=await trace.reply(listed.f,after);
      assert.ok(f.result.decisions.every(q=>q.id!==id));
    }
    console.log('real chat: confirmed option',id,option);
  }
  rig.cli(null,'away');const toOverrule=s.ask('Reverse provisional work?','--recommended','Keep this');
  await wire.frames.next(f=>f.kind==='decision.taken_provisionally'&&f.body.decision_id===toOverrule);
  const task=rig.json(null,'task','create','Redo fixture','--project','demo','--measure','{"kind":"owner"}','--rests-on',toOverrule).task_id;
  rig.json(null,'task','assign',task,'demo/worker');
  assert.equal(rig.json(null,'task','get',task).task.state,'assigned');
  rig.cli(null,'back');const toWithdraw=s.ask('Withdraw this real decision?');
  const from=trace.frames.rows.length;
  await dom(page,`#cards [data-key="${toOverrule}"] .where`,'Taken provisionally');
  await choice(card(page,toOverrule),'Overrule: Try that').click();
  assert.match(await dialog(page).innerText(),/The work done on Keep this is reopened and its owner is told\./);
  await choice(dialog(page),'Confirm overrule').click();
  const overrule=(await trace.frames.next(r=>r.dir==='send'&&r.f.method==='decision.resolve',from)).f;
  assert.deepEqual(overrule.params,{id:toOverrule,option:'Try that'});
  assert.equal((await trace.reply(overrule,from)).f.result.decision.status,'overruled');
  const fact=await wire.frames.next(f=>f.kind==='decision.overruled'&&f.body.decision_id===toOverrule);
  assert.deepEqual(fact.body.redo_tasks,[task]);
  console.log('real decisions: overrule committed',JSON.stringify({decision:toOverrule,task}));
  const reopened=rig.json(null,'task','get',task).task;
  assert.equal(reopened.state,'open'); assert.equal(reopened.agent_id,null);
  for(const to of ['owner','demo/worker']) await wire.frames.next(f=>f.kind==='msg.sent'&&f.body.to===to&&
    f.body.body.startsWith(`Task ${task} needs redo after decision ${toOverrule} was overruled: Try that`));
  await removed(page,toOverrule);
  await choice(card(page,toWithdraw),'Withdraw').click();
  await dialog(page).getByLabel('Reason (optional)').fill('Fixture is stale');
  await choice(dialog(page),'Confirm withdrawal').click();
  const withdrawal=(await trace.frames.next(r=>r.dir==='send'&&r.f.method==='decision.withdraw',from)).f;
  assert.deepEqual(withdrawal.params,{id:toWithdraw,reason:'Fixture is stale'});
  const closed=(await trace.reply(withdrawal,from)).f.result;
  assert.equal(closed.decision.status,'withdrawn');
  assert.deepEqual(closed.decision.withdrawal,{decision_id:toWithdraw,by:'owner',reason:'Fixture is stale'});
  await wire.frames.next(f=>f.kind==='decision.withdrawn'&&f.body.decision_id===toWithdraw);
  await removed(page,toWithdraw);
  assert.deepEqual(trace.sent('decision.resolve').map(f=>f.params),[{id:d,option:'Try that'},{id:p,option:'Keep this'},{id:toOverrule,option:'Try that'}]);
  assert.deepEqual((await wire.rpc('decision.list',{statuses:['pending','taken_provisionally']})).decisions,[]);
  await keepAll(page,rig,wire,trace,s.ask);
  await dom(page,'#cards .empty','Nothing needs you right now.');await select(channel);
  console.log('real chat: history, answer and keep, withdraw, overrule with committed redo and both notices');
}

async function keepAll(page,rig,wire,trace,ask) {
  const options=['Keep this','Try that','Keep this'], picks=[];
  rig.cli(null,'away');
  for(const [i,option] of options.entries()) {
    const id=ask(`Real batch choice ${i+1}`,'--recommended',option);
    const fact=await wire.frames.next(f=>f.kind==='decision.taken_provisionally'&&f.body.decision_id===id);
    assert.equal(fact.body.option,option);picks.push({id,option});
  }
  rig.cli(null,'back');const pending=ask('Pending control for the real batch');
  await dom(page,`#cards [data-key="${pending}"]`);
  await choice(page,"Keep all agents' picks (3)").click();
  const from=trace.frames.rows.length;
  assert.match(await dialog(page).innerText(),/This confirms 3 provisional picks\./);
  await choice(dialog(page),'Keep all 3 picks').click();
  await dom(page,'#needs','Kept 3 of 3 picks.');
  const sent=trace.sent('decision.resolve',from);assert.deepEqual(sent.map(f=>f.params),picks);
  const receipts=trace.frames.rows.slice(from).filter(row=>sent.some(f=>row.f?.id===f.id));
  assert.deepEqual(receipts.map(r=>r.dir),['send','recv','send','recv','send','recv']);
  for(const pick of picks) {
    const fact=await wire.frames.next(f=>f.kind==='decision.resolved'&&f.body.decision_id===pick.id);
    assert.equal(fact.body.option,pick.option);assert.equal(await card(page,pick.id).count(),0);
  }
  const open=await wire.rpc('decision.list',{statuses:['pending','taken_provisionally']});
  assert.deepEqual(open.decisions.map(d=>d.id),[pending]);assert.equal(await card(page,pending).count(),1);
  await choice(card(page,pending),'Keep this').click();await removed(page,pending);
  console.log('real decisions: three serial keeps on different picks; pending control unchanged by the batch');
}

async function submit(page,trace,body,kind='channel') {
  const at=trace.frames.rows.length;
  if(await draft(page,kind).inputValue()!==body) await draft(page,kind).fill(body);
  await send(page,kind).click();
  const {f:request}=await trace.frames.next(r=>r.dir==='send'&&r.f.method==='channel.post'&&r.f.params.body===body,at);
  assert.ok(request.params.client_key);
  const {f:answer}=await trace.reply(request,at);assert.ok(answer.result,JSON.stringify(answer));
  return {request,result:answer.result};
}

async function reconnect(page,rig,wire,trace) {
  const body='The first receipt is deliberately dropped', at=trace.frames.rows.length;
  trace.dropPost(body);await draft(page).fill(body);await send(page).click();
  const lost=await trace.frames.next(r=>r.dir==='dropped',at);
  await phase(page,'reconnecting');
  assert.equal(await draft(page).inputValue(),'','the outbox has accepted the text locally');
  const sameBody=(await wire.rpc('channel.post',{channel,body,client_key:'fixture-other-owner-send'})).post_id;
  const short=rig.json('worker','post','--channel',channel,'--body','Seat post in short gap @owner').post_id;
  trace.release();
  const watch=await trace.frames.next(r=>r.dir==='send'&&r.id>lost.id&&r.f.type==='watch',at);
  assert.equal(watch.f.cursor,lost.cursor);assert.deepEqual(watch.f.topics,topics);
  const retry=await trace.frames.next(r=>r.dir==='send'&&r.id>lost.id&&r.f.method==='channel.post'&&r.f.params.body===body,at);
  const first=trace.sent('channel.post',at).find(f=>f.id===lost.f.id);
  assert.deepEqual(retry.f.params,first.params);
  const {f:answer}=await trace.reply(retry.f,trace.frames.rows.indexOf(retry));
  const committed=lost.f.result.post_id;
  assert.equal(answer.result.post_id,committed);assert.notEqual(sameBody,committed);
  await alive(page);await shown(page,short);await shown(page,committed);await shown(page,sameBody);
  assert.deepEqual(await page.locator('#posts .msg').filter({hasText:body}).evaluateAll(rows=>rows.map(row=>row.dataset.postId)),[committed,sameBody]);
  const rows=(await wire.rpc('channel.history',{channel,limit:10})).posts;
  assert.deepEqual(rows.filter(p=>p.body===body).map(p=>p.id).sort(),[committed,sameBody].sort());
  const from=trace.frames.rows.length;trace.cut();await phase(page,'reconnecting');
  const ids=[];
  for(let i=0;i<53;i++) ids.push((await wire.rpc('channel.post',{channel,body:`Long-gap post ${i}`})).post_id);
  trace.release();await alive(page);await shown(page,ids[0]);await shown(page,ids.at(-1));
  const displayed=await page.locator('#posts [data-post-id]').evaluateAll(nodes=>nodes.map(el=>el.dataset.postId));
  assert.deepEqual(displayed.filter(id=>ids.includes(id)),ids);
  assert.ok(trace.sent('channel.history',from).some(f=>f.params.after===short));
  assert.ok(trace.sent('channel.history',from).some(f=>ids.includes(f.params.before)));
  console.log('real reconnect: automatic same-key retry, two distinct same-body rows, Watch cursor and paged catch-up');
}

async function mentionsFlow(page,rig,trace,direct) {
  const input=draft(page), menu=kind=>page.locator(`#${kind}-form`).getByRole('listbox',{name:'Mention a seat'});
  const at=trace.frames.rows.length, shot=async name=>{
    const dir=process.env.ATM_CHAT_TEST_SHOTS;
    if(dir){mkdirSync(dir,{recursive:true});await page.screenshot({path:join(dir,`${name}.png`)});}
  };
  await input.fill('');await input.pressSequentially('@');
  assert.ok(await menu('channel').isVisible(),'typing @ opens the channel picker');
  assert.equal(await menu('channel').getByRole('option',{selected:true}).innerText(),'@demo/lead');
  assert.ok(await input.evaluate(el=>el.dispatchEvent(new KeyboardEvent('keydown',
    {key:'Enter',bubbles:true,cancelable:true,isComposing:true}))), 'IME Enter stays native');
  await input.press('Escape');assert.equal(await menu('channel').isVisible(),false);
  const flagged=rig.json('worker','post','--channel',channel,'--body','Please check this @owner').post_id;
  await shown(page,flagged);assert.equal(await menu('channel').isVisible(),false,'Escape survives a repaint');

  await input.fill('');await input.pressSequentially('@');await input.press('ArrowDown');
  assert.equal(await menu('channel').getByRole('option',{selected:true}).innerText(),'@demo/worker');
  await shot('mentions-picker');await input.press('Enter');
  assert.equal(await input.inputValue(),'@demo/worker ');
  assert.ok(await input.evaluate(el=>el===document.activeElement));
  assert.equal(trace.sent('channel.post',at).length,0,'Enter selects, not sends');
  const body='@demo/worker @demo/chief @ghost';
  await input.pressSequentially('@demo/chief @ghost');
  const {result}=await submit(page,trace,body);await shown(page,result.post_id);
  const line='Woken: demo/worker · Unavailable: demo/chief · Unresolved: @ghost';
  await dom(page,'#channel-form [role=status]',line);await shot('mentions-result');
  assert.equal(trace.sent('channel.post',at).length,1);
  assert.ok(await post(page,flagged).evaluate(el=>el.classList.contains('msg--mentioned')));

  const sentRow=post(page,result.post_id);
  await choice(await messageActions(page,sentRow),'Reply in thread').tap();
  assert.equal(await page.locator('#channel-form [role=status]').textContent(),'');
  const reply=draft(page,'thread');await reply.pressSequentially('@');
  await menu('thread').getByRole('option',{name:'@demo/lead',exact:true}).tap();
  assert.equal(await reply.inputValue(),'@demo/lead ');
  assert.ok(await reply.evaluate(el=>el===document.activeElement));
  assert.equal(trace.sent('channel.post',at).length,1,'tap inserts a mention without posting');
  await submit(page,trace,'@demo/lead ','thread');await dom(page,'#thread-form [role=status]','Woken: demo/lead');
  assert.equal(trace.sent('channel.post',at).length,2);

  const dm=body=>rig.json('lead','post','--to','owner','--body',body).post_id;
  const plain=dm('Ordinary DM'), code=dm('Literal `@owner`'), mentioned=dm('DM to @owner');
  await page.locator('#thread-back').click();await page.locator('#home-back').click();
  await page.locator(`#channels [data-channel="${direct}"]`).click();await shown(page,mentioned);
  for(const [id,highlight] of [[plain,false],[code,false],[mentioned,true]])
    assert.equal(await post(page,id).evaluate(el=>el.classList.contains('msg--mentioned')),highlight);
  await page.locator('#home-back').click();await page.locator(`#channels [data-channel="${channel}"]`).click();
  console.log('real mentions: keyboard and touch, wake/down/unknown receipt, only resolved incoming owner tags highlighted');
}

async function diagnostics(page,rig,trace) {
  const at=trace.frames.rows.length;trace.cut(true);
  await phase(page,'reconnecting');
  const close=await trace.frames.next(r=>r.dir==='close'&&r.idle,at);
  // With no Connect, the bridge deadline races atmd's first read deadline.
  // The paused-clock bridge test pins 1001 when the Unix peer stays open.
  assert.ok((close.code===1001&&close.reason==='idle timeout')||
    (close.code===1011&&close.reason==='daemon disconnected'),
    `withheld Connect: unexpected close ${close.code} (${close.reason})`);
  await alive(page);
  const probe=(path,method='GET',identity)=>page.evaluate(async([url,method,identity])=>{
    const headers=identity?{'Tailscale-User-Login':identity}:{};
    const r=await fetch(url,{method,headers,cache:'no-store'});
    return [r.status,await r.json(),r.headers.get('cache-control')];
  },[path,method,identity]);
  assert.deepEqual(await probe('/health','GET','ignored-client-login'),[200,{bridge:'ok',daemon:'up',protocol:'1.1'},'no-store']);
  await rig.stopDaemon();await phase(page,'daemon-down');
  assert.deepEqual(await probe('/health'),[200,{bridge:'ok',daemon:'down',protocol:'1.1'},'no-store']);
  assert.match(await page.locator('#connection-banner').innerText(),/atm isn't running/);
  await rig.stopBridge();await phase(page,'unreachable');
  assert.match(await page.locator('#connection-banner').innerText(),/Can't reach/);
  await rig.startDaemon();await rig.startBridge('https://127.0.0.1');
  await phase(page,'origin');assert.deepEqual(await probe('/ws','POST'),[403,{error:'origin'},'no-store']);
  rig.setLogin('fixture-denied');await phase(page,'identity');
  assert.equal(await page.locator('#connection-label').textContent(),'Not allowed');
  assert.deepEqual(await probe('/health','GET',rig.login),[403,{error:'identity'},'no-store']);
  assert.deepEqual(await probe('/ws','POST'),[403,{error:'identity'},'no-store']);
  assert.equal(await page.locator('#posts article').count(),0);assert.equal(await page.evaluate(()=>localStorage.length),0);
  console.log(`real diagnostics: withheld Connect closed ${close.code} (${close.reason}) and recovered; daemon down, bridge down, Origin mismatch, identity refusal`);
}

async function desktopReturn(browser,rig,external,errors) {
  const context=await browser.newContext({viewport:{width:1897,height:1100}});
  try {
    context.on('request',request=>{if(new URL(request.url()).origin!==rig.origin)external.push(request.url());});
    const page=await context.newPage();page.on('pageerror',error=>errors.push(error.message));
    const trace=await traceSocket(page);
    await openChat(page,rig.origin);
    await page.locator(`#channels [data-channel="${channel}"]`).click();
    assert.equal(await page.evaluate(()=>navigator.maxTouchPoints),0);
    const at=trace.frames.rows.length, body='A desktop Return through the real bridge';
    await draft(page).fill(body);await draft(page).press('Enter');
    assert.notEqual(await draft(page).inputValue(),`${body}\n`);
    const {f:request}=await trace.frames.next(r=>r.dir==='send'&&r.f.method==='channel.post'&&r.f.params.body===body,at);
    assert.equal(request.params.channel,channel);assert.ok(request.params.client_key);
    const {f:answer}=await trace.reply(request,at);assert.ok(answer.result,JSON.stringify(answer));
    await shown(page,answer.result.post_id);await dom(page,'#channel-form textarea:not([readonly])');
    assert.equal(await draft(page).inputValue(),'');
    assert.equal(trace.sent('channel.post',at).length,1);
    console.log('real composer: one desktop Return, matching receipt and canonical row');
  } finally {await context.close();}
}

// Focused contract used with old and new asset bundles/binaries in the same seal.
export async function sharedReplySmoke(binaries,{daemon,app}) {
  const rig=await realRig(binaries);let browser;
  try {
    const wire=await rig.wire();
    const main={channel,limit:1,include_shared_replies:true};
    if(daemon) await wire.rpc('channel.history',main);
    else {
      await assert.rejects(wire.rpc('channel.history',main),/"code":"invalid_params".*unknown field `include_shared_replies`/);
      console.log('old daemon: strict read-only probe refused with invalid_params');
    }
    const make=(body,target={channel})=>wire.rpc('channel.post',{body,tags:['worker'],...target});
    const distant=(await make('An older root that is outside the loaded page')).post_id;
    const root=(await make('Plan the release in this thread')).post_id;
    const first=(await make('The first thread reply',{reply_to:root})).post_id;
    const {chromium}=await import(pathToFileURL(process.env.ATM_CHAT_TEST_PLAYWRIGHT));
    browser=await chromium.launch({args:['--disable-background-networking','--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1',
      `--ignore-certificate-errors-spki-list=${rig.certificatePin}`]});
    const errors=[];
    const visit=async()=>{
      const context=await browser.newContext({viewport,deviceScaleFactor:1,hasTouch:true});
      const page=await context.newPage(), trace=await traceSocket(page);
      page.on('pageerror',e=>errors.push(e.message));
      await openChat(page,rig.origin);
      await page.locator(`#channels [data-channel="${channel}"]`).click();
      return {page,trace};
    };
    const {page,trace}=await visit();
    const checkbox=page.getByRole('checkbox',{name:'Also send to chat'});
    assert.equal(await checkbox.count(),0,'not in the main composer');
    await post(page,root).getByRole('button',{name:/^1 reply/}).click();await shown(page,first,'replies');
    const enabled=daemon&&app;
    assert.equal(await checkbox.count(),Number(enabled),'thread checkbox matches both versions');
    const dir=process.env.ATM_CHAT_TEST_SHOTS;
    const image=async name=>{if(dir){mkdirSync(dir,{recursive:true});await page.screenshot({path:join(dir,`${name}.png`)});}};
    const body='The release notes are ready. @worker';
    await draft(page,'thread').fill(body);
    if(enabled){assert.equal(await checkbox.isChecked(),false);await checkbox.press('Space');assert.equal(await checkbox.isChecked(),true);}
    await image('thread-composer');
    const from=trace.frames.rows.length;
    await send(page,'thread').click();
    const {f:req}=await trace.frames.next(r=>r.dir==='send'&&r.f.method==='channel.post'&&r.f.params.body===body,from);
    assert.equal(req.params.reply_to,root);
    assert.equal(req.params.also_in_chat,enabled?true:undefined);
    const {f:receipt}=await trace.reply(req,from);assert.ok(receipt.result,JSON.stringify(receipt));
    const id=receipt.result.post_id;
    await shown(page,id,'replies');await dom(page,'#thread-form textarea:not([readonly])');
    if(enabled) assert.equal(await checkbox.isChecked(),false);
    await page.locator('#thread-back').click();
    if(enabled) {
      await shown(page,id);
      assert.equal(await post(page,id).count(),1);
      const link=post(page,id).getByRole('button',{name:/Replied in a thread.*Plan the release/});
      await link.waitFor({state:'visible'});
      await image('main-timeline');
      await link.click();await shown(page,id,'replies');await page.locator('#thread-back').click();
    } else {
      await post(page,root).getByRole('button',{name:/^2 replies/}).waitFor({state:'visible'});
      assert.equal(await post(page,id).count(),0);
      await image('main-timeline');
    }
    if(daemon&&!app) {
      const shared=await make('New client shared this reply',{reply_to:root,also_in_chat:true});
      const replies=post(page,root).getByRole('button',{name:/^3 replies/});
      await replies.waitFor({state:'visible'});assert.equal(await post(page,shared.post_id).count(),0);
      await replies.click();await shown(page,shared.post_id,'replies');
    }
    if(enabled) {
      // A fresh profile guarantees earlier catch-up did not load the distant root.
      for(let i=0;i<55;i++) await make(`Intervening main post ${i}`);
      const {page:detached,trace:track}=await visit();
      assert.equal(await post(detached,distant).count(),0);
      const a=await make('A distant shared reply',{reply_to:distant,also_in_chat:true});
      const b=await make('Another distant shared reply',{reply_to:distant,also_in_chat:true});
      const link=post(detached,b.post_id).getByRole('button',{name:/Replied in a thread.*An older root/});
      await link.waitFor({state:'visible'});await shown(detached,a.post_id);
      assert.equal(track.sent('channel.history').filter(({params:p})=>p.thread===distant&&p.limit===1).length,1);
      // A later catch-up page may also legitimately load the old root.
      await link.click();await shown(detached,distant,'replies');await shown(detached,b.post_id,'replies');
    }
    const probes=trace.sent('channel.history').filter(({params:p})=>p.include_shared_replies===true);
    if(!daemon) assert.equal(probes.length,Number(app),'only the new app probes the old daemon, once');
    else if(!app) assert.equal(probes.length,0);
    assert.equal(trace.sent('channel.post',from).length,1);
    assert.deepEqual(errors,[]);
    const plain=await wire.rpc('channel.history',{channel});
    assert.ok(plain.posts.every(row=>row.thread===null));
    console.log(`shared replies: Chromium ${browser.version()}; daemon ${daemon?'new':'old'}, app ${app?'new':'old'}; expected history, reply and navigation`);
    await wire.close();
  } finally {try{await browser?.close();}finally{await rig.close();}}
}

export async function realSmoke(binaries) {
  const rig=await realRig(binaries,{files:true});let browser;
  try {
    const wire=await rig.wire();assert.ok(wire.connected.methods.includes('org.tree'));
    const seeded=await seed(rig,wire);
    const {chromium}=await import(pathToFileURL(process.env.ATM_CHAT_TEST_PLAYWRIGHT));
    // Page-level ignoreHTTPSErrors does not suffice for worker registration.
    browser=await chromium.launch({args:['--disable-background-networking','--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1',
      `--ignore-certificate-errors-spki-list=${rig.certificatePin}`]});
    const context=await browser.newContext({viewport,deviceScaleFactor:1,hasTouch:true});
    const external=[], errors=[];
    context.on('request',request=>{if(new URL(request.url()).origin!==rig.origin)external.push(request.url());});
    const page=await context.newPage();page.on('pageerror',error=>errors.push(error.message));
    const cdp=await context.newCDPSession(page);await cdp.send('Network.enable');
    const origins=[];
    cdp.on('Network.webSocketWillSendHandshakeRequest',e=>origins.push(e.request.headers.Origin));
    const trace=await traceSocket(page);
    await chat(page,rig,wire,trace,seeded,cdp);await reconnect(page,rig,wire,trace);
    await mentionsFlow(page,rig,trace,seeded.direct);
    await photosBrowser(browser,rig,wire,seeded,dom);await wire.close();
    await desktopReturn(browser,rig,external,errors);
    await diagnostics(page,rig,trace);
    assert.ok(origins.length&&origins.every(origin=>origin===rig.origin),JSON.stringify(origins));
    assert.deepEqual(external,[]);assert.deepEqual(errors,[]);
    console.log(`real smoke: Chromium ${browser.version()}, loopback only, 412/840/1897 CSS px`);
  } finally {try {await browser?.close();} finally {await rig.close();}}
}

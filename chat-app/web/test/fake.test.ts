import { createServer } from 'node:http';
import { once } from 'node:events';
import { expect, it } from 'vitest';
import { createFake } from '../dev/fake.ts';
import { attachFake, diagnostics } from '../dev/server.mjs';
import { captured } from './fixtures/atmd-aefd6014.ts';

async function withFake(run:(api:{fake:ReturnType<typeof createFake>; rpc(method:string,params?:object):Promise<any>;
  watch(cursor:number):Promise<any>; origin:string; methods:string[]})=>Promise<void>) {
  const fake=createFake({pageSize:2});
  const server=createServer((req,res)=>{if(!diagnostics(req,res,{health:()=> 'down'})) res.writeHead(404).end();});
  const stop=attachFake(server,fake);
  const ready=once(server,'listening'); server.listen(0,'127.0.0.1'); await ready;
  const {port}=server.address() as {port:number};
  const origin=`http://127.0.0.1:${port}`;
  const ws=new WebSocket(`ws://127.0.0.1:${port}/ws`);
  const frame=()=>new Promise<any>((resolve,reject)=>{
    const closed=()=>reject(new Error('Socket closed while awaiting frame'));
    ws.addEventListener('close',closed,{once:true});
    ws.addEventListener('message',e=>{ws.removeEventListener('close',closed); resolve(JSON.parse(e.data));},{once:true});
  });
  const send=(p:object)=>ws.send(JSON.stringify({protocol_version:{major:1,minor:1},...p}));
  const exchange=(p:object)=>{const next=frame();send(p);return next;};
  try {
    await once(ws,'open'); const {methods}=await exchange({type:'connect'});
    await run({fake,methods,origin,rpc:(method,params={})=>exchange({type:'request',id:'r',method,params}),
      watch:cursor=>exchange({type:'watch',topics:['channel.','decision.','msg.'],cursor})});
  } finally { stop(); server.closeAllConnections(); await new Promise<void>(r=>server.close(()=>r())); }
}

it('lists canonical owner/read-only/system rows and projects legacy posts', async ()=>withFake(async ({rpc})=>{
  const list=(await rpc('channel.list')).result;
  expect(list.channels.map((r:any)=>[r.id,r.section,r.writable])).toEqual(captured.list.result.channels.map(r=>[r.id,r.section,r.writable]));
  const project=list.channels[0];
  expect(Object.keys(project).sort()).toEqual(Object.keys(captured.list.result.channels[0]!).sort());
  expect(project.members[0]).toEqual({address:'owner',state:null});
  expect(project.former).toEqual(['demo/chief']);
  expect(project.seen_through).toBeNull();
  expect(list.channels.find((c:any)=>!c.writable)).toMatchObject({member:false,unread:0});
  const legacy=(await rpc('channel.history',{channel:'owner+system'})).result.posts[0];
  expect(legacy).toMatchObject({author:'system',legacy:true,woke:['owner'],thread:null,session_id:null,decision:null});
}));

it('keeps the first post for a key and replays only events beyond the cursor', async ()=>withFake(async ({rpc,watch,origin,methods})=>{
  expect(methods).toEqual(['channel.list','channel.history','channel.post','channel.seen','decision.list','decision.resolve','decision.withdraw',
    'channel.edit','channel.remove','channel.react']);
  const list=(await rpc('channel.list')).result;
  const params={channel:'demo',body:'One durable post',client_key:'same-key'};
  const first=(await rpc('channel.post',params)).result;
  expect(Object.keys(first).sort()).toEqual(Object.keys(captured.post.result).sort());
  expect(first.thread).toBeNull();
  expect((await rpc('channel.post',{...params,body:'Different input'})).result.post_id).toBe(first.post_id);
  const page=(await rpc('channel.history',{channel:'demo',limit:50})).result;
  expect(page.more).toBe(true);
  expect(page.posts.filter((p:any)=>p.body==='One durable post').map((p:any)=>p.id)).toEqual([first.post_id]);
  expect(page.posts[0]).toMatchObject({kind:'note',thread:null});
  expect(page.posts[0]).not.toHaveProperty('client_key');
  expect(await rpc('channel.post',{...params,reply_to:first.post_id})).toMatchObject({error:{code:'invalid_params'}});
  expect(await rpc('channel.post',{channel:'not-a-real-channel',body:'Unknown'})).toMatchObject({error:{code:'unknown_channel'}});
  expect(await rpc('channel.post',{channel:'demo/lead+demo/worker',body:'Non-member'})).toMatchObject({error:{code:'refused'}});
  expect(await watch(list.seq)).toMatchObject({kind:'channel.posted',type:'event',seq:list.seq+1});
  expect(await (await fetch(`${origin}/health`)).json()).toEqual({bridge:'ok',daemon:'down',protocol:'1.1'});
  expect(await (await fetch(`${origin}/ws`,{method:'POST'})).json()).toEqual({error:'origin'});
}));

it('serves edit/remove as strict, non-waking mutations of the same owner post and thread', async ()=>withFake(async ({rpc,fake})=>{
  const {post_id:post}= (await rpc('channel.post',{channel:'demo',body:'Original',attachments:['note.txt']})).result;
  await rpc('channel.post',{reply_to:post,body:'A reply'});
  const denied=fake.inject('demo','From a seat');
  const legacy=fake.inject('demo','From the legacy log',{legacy:true,author:'owner'});
  const before=(await rpc('channel.list')).result, events:unknown[]=[];
  const stop=fake.watch(before.seq,e=>events.push([e.kind,e.body]));
  const page=async()=>(await rpc('channel.history',{channel:'demo',thread:post,limit:2})).result.posts;
  try {
    expect(await rpc('channel.edit',{post,body:'Edited',channel:'demo'})).toMatchObject({error:{code:'invalid_params'}});
    expect(await rpc('channel.remove',{post:'m99999'})).toMatchObject({error:{code:'not_found'}});
    for(const row of [denied,legacy]) expect(await rpc('channel.remove',{post:row?.id})).toMatchObject({error:{code:'refused'}});
    expect((await rpc('channel.edit',{post,body:'Edited'})).result).toEqual({seq:before.seq+1,post_id:post});
    expect((await page())[0]).toMatchObject({id:post,edited:true,body:'Edited',attachments:['note.txt'],replies:1});
    expect((await rpc('channel.remove',{post})).result).toEqual({seq:before.seq+2,post_id:post});
    expect(await page()).toMatchObject([{id:post,body:'removed',removed:true,attachments:[],mentions:[],task:null,replies:1},
      {thread:post,body:'A reply'}]);
    expect((await rpc('channel.list')).result.channels).toEqual(before.channels);
    expect(events).toEqual([['channel.post_edited',{post,actor:'owner',body:'Edited'}],['channel.post_removed',{post,actor:'owner'}]]);
  } finally {stop();}
}));

it('toggles one actor by the canonical post or own wake without waking anyone',async()=>withFake(async({rpc,fake})=>{
  const row=fake.inject('demo','Agent peer',{woke:['owner']});
  if(!row) throw new Error('missing synthetic channel');
  const before=(await rpc('channel.list')).result, post=row.id, wake=`m${before.seq}`;
  const events:unknown[]=[];const stop=fake.watch(before.seq,e=>events.push([e.kind,e.body]));
  const page=async()=>(await rpc('channel.history',{channel:'demo',limit:1})).result.posts.at(0);
  try {
    expect(await rpc('channel.react',{post,emoji:'x'})).toMatchObject({error:{code:'invalid_params'}});
    expect(await rpc('channel.react',{post,emoji:'👍',actor:'demo/lead'})).toMatchObject({error:{code:'invalid_params'}});
    expect((await rpc('channel.react',{post:wake,emoji:'👍'})).result).toEqual({seq:before.seq+1,post_id:post});
    fake.reactAs(post,'👍','demo/worker');fake.reactAs(post,'✅','demo/worker');
    expect((await page()).reactions).toEqual([{emoji:'👍',count:2,mine:true},{emoji:'✅',count:1,mine:false}]);
    expect((await rpc('channel.react',{post,emoji:'👍'})).result).toEqual({seq:before.seq+4,post_id:post});
    expect((await page()).reactions).toEqual([{emoji:'👍',count:1,mine:false},{emoji:'✅',count:1,mine:false}]);
    expect((await rpc('channel.list')).result.channels).toEqual(before.channels);
    expect(events).toEqual([
      ['channel.reacted',{post,actor:'owner',emoji:'👍',active:true}],['channel.reacted',{post,actor:'demo/worker',emoji:'👍',active:true}],
      ['channel.reacted',{post,actor:'demo/worker',emoji:'✅',active:true}],['channel.reacted',{post,actor:'owner',emoji:'👍',active:false}],
    ]);
  } finally {stop();}
}));

it('pages thread root and replies with exclusive bounds and a stable marker', async ()=>withFake(async ({rpc,fake})=>{
  const root=fake.inject('demo','Unread only after a marker');
  const tagged=fake.inject('demo','A post for @owner',{mentions:['owner'],woke:['owner']});
  if(!root||!tagged) throw new Error('missing synthetic project');
  const channel=async()=>(await rpc('channel.list')).result.channels.find((r:any)=>r.id==='demo');
  expect(await channel()).toMatchObject({unread:1,seen_through:null});
  const marker=(await rpc('channel.seen',{channel:'demo',through:root.id})).result;
  expect(marker).toMatchObject({changed:true,through:root.id,seq:expect.any(Number)});
  expect((await rpc('channel.seen',{channel:'demo',through:root.id})).result).toEqual({...marker,changed:false});
  expect(await channel()).toMatchObject({unread:1,seen_through:root.id});
  for(const body of ['Reply A','Reply B']) await rpc('channel.post',{reply_to:root.id,body});
  const params={channel:'demo',thread:root.id,limit:2};
  const first=(await rpc('channel.history',params)).result;
  expect(first.posts.map((p:any)=>p.body)).toEqual(['Unread only after a marker','Reply A']);
  expect(first.posts[0]).toMatchObject({replies:2,thread:null});
  expect(first.more).toBe(true);
  const next=(await rpc('channel.history',{...params,after:first.posts[1].id})).result;
  expect(next.posts.map((p:any)=>p.body)).toEqual(['Reply B']);
  expect(next.more).toBe(false);
  const earlier=(await rpc('channel.history',{...params,before:first.posts[1].id})).result;
  expect(earlier.posts.map((p:any)=>p.id)).toEqual([root.id]);
}));

it('paginates unlinked decisions and resolves an option without adding a project post', async ()=>withFake(async ({rpc})=>{
  const page=(await rpc('decision.list',{limit:1})).result;
  const [question]=page.decisions;
  expect(Object.keys(question).sort()).toEqual(Object.keys(captured.decisionFirstPage.result.decisions[0]!).sort());
  expect(question).not.toHaveProperty('post_id');
  expect(question).not.toHaveProperty('channel');
  expect(page.next_after).toBe(question.id);
  const last=(await rpc('decision.list',{limit:1,after:page.next_after})).result;
  expect(last.next_after).toBeNull();
  expect(last.decisions[0].status).toBe('taken_provisionally');
  const lastActivity=async()=>(await rpc('channel.list')).result.channels.find((c:any)=>c.id==='demo').activity_seq;
  const activity=await lastActivity();
  const params={id:question.id,option:question.request.options[0]};
  expect((await rpc('decision.resolve',params)).result).toMatchObject({changed:true,decision:{status:'resolved',resolution:{by:'owner',option:params.option}}});
  expect((await rpc('decision.resolve',params)).result.changed).toBe(false);
  expect(await lastActivity()).toBe(activity);
}));

it('refuses withdrawal after a take and keeps the two closing operations distinct', async ()=>withFake(async ({rpc,fake})=>{
  const pending=fake.ask('Withdraw this sample',false), taken=fake.ask('Overrule this sample',true);
  expect(await rpc('decision.withdraw',{id:taken})).toMatchObject({error:{code:'refused'}});
  const params={id:pending,reason:'Fixture is stale'};
  expect((await rpc('decision.withdraw',params)).result).toMatchObject({changed:true,decision:{status:'withdrawn',withdrawal:{by:'owner',reason:params.reason}}});
  expect((await rpc('decision.withdraw',params)).result.changed).toBe(false);
  expect((await rpc('decision.resolve',{id:taken,option:'Second sample'})).result)
    .toMatchObject({changed:true,decision:{status:'overruled',resolution:{option:'Second sample'}}});
}));

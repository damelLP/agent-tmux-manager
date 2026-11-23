import { afterEach, expect, it, vi } from 'vitest';
import { mentionChoices, sendControl } from '../src/composer.ts';
import { fileHead as fetchHead, upload as fetchUpload } from '../src/photos.ts';
import { createSession, diagnose, resetForReplay } from '../src/session.ts';
import type { SessionIO, SocketEvents } from '../src/session.ts';
import { initial, reduce, roots, scopeKey, threadHead } from '../src/state.ts';
import type { ChatState, DecisionAction, OutboxItem } from '../src/state.ts';
import { captured } from './fixtures/atmd-aefd6014.ts';

const channel = {id:'demo',section:'channel',kind:'project',activity_seq:15,unread:2,writable:true,members:[{address:'owner'}],former:[]};
const row = (n:number, extra={}) => ({id:`m${n}`,channel:'demo',author:'demo/lead',kind:'note',body:`Note ${n}`,
  posted_at:'2026-01-01T00:00:00Z',replies:0,attachments:[],mentions:[],...extra});
const q = {id:'d8',post_id:'m9',channel:'demo',status:'pending',request:{requester:'demo/lead',title:'Ready?',context:'',options:['Yes','No']}};
const key = scopeKey('demo');
const answer=(option:string,id:`d${bigint}`='d8')=>({type:'decision',action:{id,option}} as const);
const drain = () => new Promise<void>(resolve=>setImmediate(resolve)); // flush the promise jobs of an in-memory RPC
const sessions:ReturnType<typeof createSession>[] = [];
afterEach(()=>{ for (const s of sessions.splice(0)) s.stop(); vi.useRealTimers(); vi.unstubAllGlobals(); });

function fixture(seed?:ChatState,onNavigate?:SessionIO['onNavigate'],agentList=false, outbox?:{save:(entries:readonly OutboxItem[])=>boolean; owned?:boolean}) {
  let s:ChatState;
  const renders:ChatState[]=[];
  let frameHandler:(p:any,reply:(value:unknown)=>void,drop:()=>void,refuse:(code?:string)=>void)=>void = (p,reply) => {
    if (p.method === 'channel.list') reply({seq:15,channels:[channel]});
    if (p.method === 'channel.history') reply({seq:15,channel:'demo',posts:p.params.after ? [] : [row(10),row(9)],more:false});
    if (p.method === 'decision.list') reply({decisions:[q],next_after:null});
  };
  let foreground=true, online=true;
  let pong:(send:()=>void)=>void = send=>send();
  let offered:string[]=[];
  let beforeSend:(frame:any)=>void=()=>{};
  let probe:SessionIO['probe'] = async ()=> 'reconnecting';
  let keys=0, uuid=()=> keys++===0 ? 'a-deduplication-key' : `key-${keys}`;
  let upload:SessionIO['upload'] = async ()=> {throw new Error('Unexpected upload');};
  const sockets:SocketEvents[] = [], sent:any[] = [];
  const client = createSession({
    socket(events) { sockets.push(events); return {close(){},send(text) {
      const p = JSON.parse(text); beforeSend(p); sent.push(p);
      const respond = (f:unknown)=>events.message(JSON.stringify(f));
      if (p.type === 'connect') respond({type:'connected',protocol_version:{major:1,minor:1},client_id:'demo',methods:[
        'channel.list','channel.history','channel.post','channel.seen','decision.list','decision.resolve','decision.withdraw',
        ...(agentList?['agent.list']:[]),...offered]});
      if (p.type === 'ping') pong(()=>respond({type:'pong',seq:p.seq}));
      if (p.type === 'request') frameHandler(p,result=>respond({type:'response',id:p.id,result}),events.close,
        (code='refused')=>respond({type:'response',id:p.id,error:{code,message:'Synthetic diagnostic'}}));
    }}; },
    timer(fn,ms) { const id=setTimeout(fn,ms); return ()=>clearTimeout(id); },
    online:()=>online, foreground:()=>foreground, probe:signal=>probe(signal), uuid:()=>uuid(),onNavigate,
    upload:(blob,name,signal)=>upload(blob,name,signal),fileHead:fetchHead,saveOutbox:outbox?.save??(()=>true),purgeOutbox:()=>true,
  },next=>{s=next;renders.push(next);},seed);
  sessions.push(client);
  const event = (seq:number,kind='msg.sent',body:object={bad:'not a post'})=>sockets.at(-1)?.message(JSON.stringify({type:'event',seq,kind,body}));
  async function start() {
    client.start();
    if(outbox?.owned!==false) client.dispatch({type:'outbox-open',items:seed?.outbox??[]});
    sockets.at(-1)?.open(); await drain();
  }
  return {client,sockets,sent,renders,event,start,state:()=>s,set offered(value:string[]){offered=value;},set frames(fn:typeof frameHandler){frameHandler=fn;},
    set foreground(v:boolean){foreground=v;}, set online(v:boolean){online=v;},set beforeSend(fn:typeof beforeSend){beforeSend=fn;},
    set probe(fn:typeof probe){probe=fn;},set pong(fn:typeof pong){pong=fn;},
    set upload(fn:typeof upload){upload=fn;},set uuid(fn:typeof uuid){uuid=fn;}};
}

const editPost=(post:`m${bigint}`,body:string)=>({type:'post-change',method:'channel.edit',params:{post,body}} as const);
const removePost=(post:`m${bigint}`)=>({type:'post-change',method:'channel.remove',params:{post}} as const);
const ownerRow=(n:number,extra={})=>row(n,{author:'owner',legacy:false,...extra});
const mutationCalls=(h:ReturnType<typeof fixture>)=>h.sent.filter(f=>['channel.edit','channel.remove','channel.react'].includes(f.method));

it('keeps old hello usable and gates each mutation on its own advertised method',async()=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture();let post=ownerRow(10),seq=15;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[channel]});
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:p.params.after?[]:[post],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.edit') {post={...post,body:p.params.body,edited:true};reply({seq:++seq,post_id:'m10'});}
  };
  await h.start();expect(h.state().phase).toBe('live');
  h.client.dispatch(editPost('m10','Old'));h.client.dispatch(removePost('m10'));
  expect(mutationCalls(h)).toEqual([]);
  h.offered=['channel.edit'];h.sockets.at(-1)?.close();await drain();
  vi.advanceTimersByTime(1000);h.sockets.at(-1)?.open();await drain();
  h.client.dispatch(removePost('m10'));h.client.dispatch(editPost('m10','  new copy  '));
  h.client.dispatch(editPost('m10','Double click'));
  await drain();
  expect(mutationCalls(h).map(f=>[f.method,f.params])).toEqual([['channel.edit',{post:'m10',body:'  new copy  '}]]);
  expect(h.state().histories.get(key)?.rows.get('m10')).toMatchObject({seq:16,row:{body:'  new copy  ',edited:true}});
});

it.each(['offline','notSent'])('acknowledges an edit only once the socket accepts it (%s)',async(refusal)=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture();h.offered=['channel.edit'];
  let replyToEdit:((value:unknown)=>void)|undefined;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:p.params.after?[]:[ownerRow(10)],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.edit') replyToEdit=reply;
  };
  await h.start();
  const onSent=vi.fn(), change={...editPost('m10','Keep this draft'),onSent};
  if(refusal==='offline') h.online=false;
  else h.beforeSend=f=>{if(f.method==='channel.edit') throw new Error('Socket closed before send');};
  h.client.dispatch(change);await drain();
  expect(onSent).not.toHaveBeenCalled();expect(mutationCalls(h)).toEqual([]);
  h.online=true;h.beforeSend=()=>{};
  if(refusal==='notSent') {vi.advanceTimersByTime(1000);h.sockets.at(-1)?.open();await drain();}
  expect(mutationCalls(h)).toEqual([]);
  h.client.dispatch(change);
  expect(replyToEdit).toBeDefined();
  expect(onSent).toHaveBeenCalledTimes(1); // The server receipt is still held.
  expect(mutationCalls(h).map(f=>f.params)).toEqual([{post:'m10',body:'Keep this draft'}]);
});

it('rehydrates a cached row and rolls it back at the same cursor without following unsupported raw watch frames',async()=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  let seed=reduce(initial(),{type:'channels',seq:15,rows:[channel as never]});
  seed=reduce(seed,{type:'history',seq:-1,key,rows:[ownerRow(9,{body:'Stale cache'}) as never]});
  const h=fixture(seed);h.offered=['channel.edit','channel.remove'];
  let seq=15,post=ownerRow(9,{body:'Edited on daemon',edited:true});
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[channel]});
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:p.params.after?[]:[post],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
  };
  await h.start();
  expect(h.state().phase).toBe('live');
  expect(roots(h.state(),key)).toMatchObject([{id:'m9',body:'Edited on daemon',edited:true}]);
  expect(h.sent).toContainEqual(expect.objectContaining({method:'channel.history',params:{channel:'demo',before:'m10',limit:1,include_shared_replies:true}}));
  post=ownerRow(9,{body:'Original from old daemon'});h.offered=[];
  h.sockets.at(-1)?.close();await drain();vi.advanceTimersByTime(1000);h.sockets.at(-1)?.open();await drain();
  expect(h.sent.filter(f=>f.type==='watch').map(f=>f.cursor)).toEqual([15,15]);
  expect(roots(h.state(),key)).toMatchObject([{id:'m9',body:'Original from old daemon'}]);
  expect(roots(h.state(),key)[0]).not.toHaveProperty('edited');
  expect(h.state().postMethods).toEqual([]);
  const before=h.sent.length;
  h.event(++seq,'channel.post_edited',{post:'m9',body:'Not a projection'});
  h.client.dispatch(editPost('m9','No longer supported'));await drain();
  expect(h.sent.slice(before)).toEqual([]);
  h.sockets.at(-1)?.close();await drain();vi.advanceTimersByTime(1000);h.sockets.at(-1)?.open();await drain();
  // The ignored mutation advanced the cursor, but caused no history or list request.
  expect(h.sent.filter(f=>f.type==='watch').map(f=>f.cursor)).toEqual([15,15,16]);
});

it('coalesces mutations during a pending page and refreshes only matching shared reply, thread, and quote copies',async()=>{
  const h=fixture();h.offered=['channel.edit','channel.remove'];
  const quote=ownerRow(8,{body:'Old head',replies:2}), reply=ownerRow(10,{thread:'m8',body:'Old reply'});
  let changed=false,hold=false,release:undefined|(()=>void);
  const threadKey=scopeKey('demo','m8');
  h.frames=(f,respond)=>{
    const p=f.params;
    if(f.method==='channel.list') respond({seq:changed?23:20,channels:[channel]});
    if(f.method==='decision.list') respond({decisions:[]});
    if(f.method!=='channel.history') return;
    const nested=changed?{...reply,body:'New reply',edited:true}:reply;
    const head=changed?{...quote,body:'removed',removed:true}:quote;
    const shared={...nested,also_in_chat:true};
    const seq=changed?23:20;
    const posts=p.thread?(p.after?[nested]:p.limit===1?[head]:[head,nested]):p.after?[]:[shared];
    const send=()=>respond({seq,channel:'demo',posts,more:false});
    if(p.thread&&p.limit!==1&&hold) {hold=false;release=send;} else send();
  };
  await h.start();expect(threadHead(h.state(),'demo','m8')?.body).toBe('Old head');
  hold=true;h.client.dispatch({type:'thread',id:'m8'});await drain();
  expect(release).toBeDefined();const start=h.sent.length;changed=true;
  h.event(21,'channel.post_edited',{post:'m10'});h.event(22,'channel.post_edited',{post:'m10'});
  h.event(23,'channel.post_removed',{post:'m8'});await drain();
  expect(h.sent.slice(start)).toEqual([]);
  release?.();await drain();
  expect(h.sent.slice(start).filter(f=>f.method==='channel.history').map(f=>f.params)).toEqual([
    {channel:'demo',before:'m11',limit:1,include_shared_replies:true},{channel:'demo',thread:'m8',after:'m9',limit:1},
    {channel:'demo',thread:'m8',limit:1},
  ]);
  expect(roots(h.state(),key)).toMatchObject([{id:'m10',body:'New reply',shared:true}]);
  expect(roots(h.state(),threadKey)).toMatchObject([{id:'m10',body:'New reply',thread:'m8'}]);
  expect(threadHead(h.state(),'demo','m8')).toMatchObject({id:'m8',body:'removed',removed:true,replies:2});
});

it.each(['disconnect','timeout'])('reads history after an ambiguous reaction (%s) without replaying the toggle',async(end)=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture();h.offered=['channel.react'];
  const toggle={type:'post-change',method:'channel.react',params:{post:'m10',emoji:'👍'}} as const;
  let seq=15,mine=false,release:undefined|(()=>void);
  h.frames=(f,reply,drop,refuse)=>{
    if(f.method==='channel.list') reply({seq,channels:[channel]});
    if(f.method==='decision.list') reply({decisions:[]});
    if(f.method==='channel.history') {
      const send=()=>reply({seq,channel:'demo',posts:f.params.after?[]:[row(10,{legacy:false,reactions:[{emoji:'👍',count:mine?2:1,mine}]})],more:false});
      if(f.params.before==='m11'&&f.params.limit===1&&mine&&!release) release=send; else send();
    }
    if(f.method==='channel.react') {mine=!mine;seq++;if(end==='disconnect') drop();else refuse('timeout');}
  };
  await h.start();h.client.dispatch(toggle);await drain();
  expect(mutationCalls(h).map(f=>f.params)).toEqual([{post:'m10',emoji:'👍'}]);
  if(end==='disconnect') {
    expect(h.state().phase).toBe('reconnecting');h.client.dispatch(toggle);
    vi.advanceTimersByTime(1000);h.sockets.at(-1)?.open();await drain();
  }
  expect(release).toBeDefined();h.client.dispatch(toggle);await drain();
  expect(mutationCalls(h)).toHaveLength(1);
  release?.();await drain();
  expect(h.state().phase).toBe('live');
  expect(roots(h.state(),key)[0]?.reactions).toEqual([{emoji:'👍',count:2,mine:true}]);
});

const lead='demo/lead', worker='demo/worker';
const activityDm=(...members:string[])=>({...channel,id:members.join('+'),section:'direct',kind:'direct',
  members:members.map(address=>({address})),writable:members.includes('owner')});
const direct=activityDm(lead,'owner'), secondDm=activityDm(worker,'owner'), observerDm=activityDm(lead,worker), systemDm=activityDm('owner','system');
const seat=(agent_id=lead,activity:string|null='working',state='ready')=>({agent_id,state,activity,
  pane:'%1',session_id:'fixture',harness:'pi',model:'fixture',cost_usd:0,reason:null});
function activityFixture(supports=true) {
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture(undefined,undefined,supports);
  let seq=100, posts:ReturnType<typeof row>[]=[], respond=(reply:(v:unknown)=>void,_refuse:(code?:string)=>void)=>reply([]);
  let listReply:(send:()=>void)=>void=send=>send();
  h.frames=(p,reply,_drop,refuse)=>{
    const {channel:id,thread,after,before,limit,include_shared_replies}=p.params;
    switch(p.method) {
      case 'channel.list': listReply(()=>reply({seq,channels:[channel,direct,secondDm,observerDm,systemDm].map(c=>({...c,activity_seq:seq}))})); break;
      case 'channel.history': {
        const n=(id:string)=>Number(id.slice(1));
        const rows=posts.filter((r:any)=>r.channel===id && (thread?r.id===thread||r.thread===thread:!r.thread||include_shared_replies&&r.also_in_chat)
          && (!after||n(r.id)>n(after))&&(!before||n(r.id)<n(before)))
          .sort((a,b)=>(thread?1:-1)*(n(a.id)-n(b.id)));
        reply({seq,channel:id,posts:rows.slice(0,limit).map(r=>thread?{...r,also_in_chat:undefined}:r),more:rows.length>limit}); break;
      }
      case 'decision.list': reply({decisions:[]}); break;
      case 'agent.list': respond(reply,refuse); break;
    }
  };
  const event=(kind:string)=>{h.event(++seq,kind);return drain();};
  const select=(id:string)=>{h.client.dispatch({type:'select',channel:id});return drain();};
  const wake=()=>{h.client.dispatch({type:'wake'});return drain();};
  const advance=(ms:number)=>{vi.advanceTimersByTime(ms);return drain();};
  return {h,event,select,wake,advance,read:()=>h.sent.filter(f=>f.method==='agent.list'),
    marks:()=>[...h.state().activity??[]],set posts(rows:typeof posts){posts=rows;},
    set activity(fn:typeof respond){respond=fn;},set list(fn:typeof listReply){listReply=fn;}};
}

it('refreshes DM activity on the two watch events and checks again at 10 s only while working',async()=>{
  const f=activityFixture(); let busy:string|null='idle';
  f.activity=reply=>reply([seat(worker),seat(lead,busy)]);
  await f.h.start();expect(f.read()).toHaveLength(0);
  await f.select(direct.id);expect(f.read().map(p=>p.params)).toEqual([{}]);expect(f.marks()).toEqual([]);
  busy='working';await f.event('decision.requested');expect(f.read()).toHaveLength(1);
  await f.event('msg.delivered');expect(f.marks()).toEqual([[lead,'working']]);
  await f.advance(9_999);expect(f.read()).toHaveLength(2);
  busy='idle';const from=f.h.sent.length;
  await f.advance(1);expect(f.h.sent.slice(from).map(f=>f.method)).toEqual(['agent.list']);expect(f.marks()).toEqual([]);
  await f.advance(10_000);expect(f.read()).toHaveLength(3);
  busy='working';await f.event('msg.delivered');expect(f.marks()).toEqual([[lead,'working']]);
  busy='attention_needed';await f.event('channel.posted');expect(f.marks()).toEqual([[lead,'attention_needed']]);
  await f.advance(10_000);expect(f.read()).toHaveLength(5);
  busy='idle';await f.event('channel.posted');expect(f.marks()).toEqual([]);
});

it('uses the latest owner post in the main timeline and the latest owner reply in each thread',async()=>{
  const f=activityFixture(), tag=(...ids:string[])=>ids.map(seat=>({seat}));
  const root=row(8,{author:'owner',mentions:tag(lead),replies:1}), sibling=row(9,{author:'owner',mentions:tag(worker)});
  const shared=row(12,{thread:'m8',also_in_chat:true,author:'owner',mentions:[...tag(worker,lead,worker),
    'owner',{group:{project:'demo',name:'builders'}}]});
  const other=row(13,{mentions:tag('demo/unrelated')});
  f.posts=[root,sibling,shared,other];
  f.activity=reply=>reply([seat(lead),seat(worker,'attention_needed'),seat('demo/unrelated')]);
  await f.h.start();await f.select('demo');
  expect(f.marks()).toEqual([[worker,'attention_needed'],[lead,'working']]);
  f.h.client.dispatch({type:'thread',id:'m8'});await drain();expect(f.marks()).toEqual([[worker,'attention_needed'],[lead,'working']]);
  f.h.client.dispatch({type:'thread',id:'m9'});await drain();expect(f.marks()).toEqual([]); // The root's tag is not a thread reply.
  const before=f.read().length;
  f.posts=[root,sibling,shared,other,row(14,{author:'owner',body:'Unresolved @lead',mentions:[]})];
  await f.select('demo');await f.event('channel.posted');
  expect(f.marks()).toEqual([]);expect(f.read()).toHaveLength(before);
});

it('renders nothing for a down, stuck, idle, missing or system peer',async()=>{
  const f=activityFixture();
  f.activity=reply=>reply([seat()]);
  await f.h.start();await f.select(direct.id);expect(f.marks()).toEqual([[lead,'working']]);
  for(const rows of [[seat(lead,'working','down')],[seat(lead,'attention_needed','stuck')],[seat(lead,'idle')],[],[seat(worker)]]) {
    f.activity=reply=>reply(rows);await f.event('channel.posted');expect(f.marks()).toEqual([]);
  }
  const before=f.read().length;
  for(const c of [observerDm,systemDm]) {await f.select(c.id);await f.event('msg.delivered');expect(f.marks()).toEqual([]);}
  expect(f.read()).toHaveLength(before);
});

it.each(['hide','Back','switch','stop'])('cancels the working tick on %s',async(end)=>{
  const f=activityFixture();f.activity=reply=>reply([seat()]);
  await f.h.start();await f.select(direct.id);expect(f.marks()).toEqual([[lead,'working']]);
  if(end==='hide'){f.h.foreground=false;await f.wake();}
  if(end==='Back')f.h.client.dispatch({type:'restore',view:{pane:'home',channel:direct.id}});
  if(end==='switch')await f.select(secondDm.id);
  if(end==='stop')f.h.client.stop();
  expect(f.marks()).toEqual([]);
  const count=f.read().length;await f.advance(10_000);expect(f.read()).toHaveLength(count);
});

it.each(['navigation','visibility'])('drops the old in-flight sample across a round trip in %s',async(source)=>{
  const f=activityFixture();const held:((v:unknown)=>void)[]=[];
  f.activity=reply=>held.push(reply);
  await f.h.start();await f.select(direct.id);expect(held).toHaveLength(1);
  if(source==='navigation'){await f.select(secondDm.id);await f.select(direct.id);}
  else {f.h.foreground=false;await f.wake();f.h.foreground=true;await f.wake();}
  expect(f.read()).toHaveLength(1);
  const painted=f.h.renders.length;
  held[0]([seat()]);await drain();
  expect(f.h.renders.slice(painted).some(s=>s.activity?.has(lead))).toBe(false);
  expect(held).toHaveLength(2);
  held[1]([seat(lead,'attention_needed')]);await drain();expect(f.marks()).toEqual([[lead,'attention_needed']]);
});

it('coalesces a due activity tick and a burst into one read at a time',async()=>{
  const f=activityFixture();f.activity=reply=>reply([seat()]);
  await f.h.start();await f.select(direct.id);
  const held:((v:unknown)=>void)[]=[];f.activity=reply=>held.push(reply);
  await f.advance(10_000);expect(held).toHaveLength(1);
  await Promise.all([f.event('msg.delivered'),f.event('channel.posted'),f.event('msg.delivered')]);
  expect(held).toHaveLength(1);
  held[0]([seat()]);await drain();expect(held).toHaveLength(2);
  held[1]([seat(lead,'idle')]);await drain();expect(f.marks()).toEqual([]);
  await f.advance(10_000);expect(held).toHaveLength(2);
});

it('keeps the 10 s check while an unrelated refresh waits on channel.list',async()=>{
  const f=activityFixture();f.activity=reply=>reply([seat()]);
  await f.h.start();await f.select(direct.id);expect(f.marks()).toEqual([[lead,'working']]);
  let release:()=>void=()=>{};f.list=send=>{release=send;};
  await f.event('decision.requested');f.activity=reply=>reply([seat(lead,'idle')]);
  await f.advance(10_000);expect(f.read()).toHaveLength(2);expect(f.marks()).toEqual([]);
  release();await drain();expect(f.read()).toHaveLength(2);
});

it.each(['refused','malformed','timeout'])('clears a stale mark on %s without losing the live link',async(failure)=>{
  const f=activityFixture();f.activity=reply=>reply([seat()]);
  f.posts=[row(10,{channel:direct.id})];
  await f.h.start();await f.select(direct.id);expect(f.marks()).toEqual([[lead,'working']]);
  const key=scopeKey(direct.id);
  f.h.client.dispatch({type:'draft',key,text:'Keep this draft',start:0,end:4});
  let late:(v:unknown)=>void=()=>{};
  f.activity=(reply,refuse)=>{
    if(failure==='refused')refuse();else if(failure==='malformed')reply({agents:[]});else late=reply;
  };
  await f.event('channel.posted');if(failure==='timeout')await f.advance(30_000);
  expect(f.marks()).toEqual([]);expect(f.h.state().phase).toBe('live');expect(f.h.sockets).toHaveLength(1);
  expect(f.h.state().drafts.get(key)?.text).toBe('Keep this draft');expect(roots(f.h.state(),key).map(r=>r.id)).toEqual(['m10']);
  const count=f.read().length;late([seat()]);await f.advance(10_000);
  expect(f.marks()).toEqual([]);expect(f.read()).toHaveLength(count);
});

it('loads a cold writable project roster once without watching seats or starting a poll',async()=>{
  const f=activityFixture();let reply:(v:unknown)=>void=()=>{};
  f.activity=send=>{reply=send;};
  await f.h.start();expect(f.read()).toHaveLength(0);
  await f.select(observerDm.id);expect(f.read()).toHaveLength(0);
  await f.select('demo');expect(f.read()).toHaveLength(1);
  expect(f.h.state().phase).toBe('live');
  reply([seat(worker),seat('else/lead',null,'down')]);await drain();
  expect(f.h.state().seatIds).toEqual([worker,'else/lead']);
  expect(f.marks()).toEqual([]);
  await f.event('channel.posted');await f.advance(10_000);expect(f.read()).toHaveLength(1);
  f.h.sockets[0].close();await drain();expect(f.h.state().seatIds).toBeUndefined();
});

it.each([false,true])('uses only members if the optional roster is absent or refused (advertised: %s)',async supported=>{
  const f=activityFixture(supported);f.activity=(_reply,refuse)=>refuse();
  await f.h.start();await f.select(direct.id);
  expect(f.read()).toHaveLength(supported?1:0);
  const s=f.h.state();expect(s.phase).toBe('live');
  expect(mentionChoices(s.channels.find(c=>c.id===direct.id),s.seatIds??[],'')).toEqual([lead]);
});

it('does not paint in-flight working status or roster on the replacement link',async()=>{
  const f=activityFixture(), held:((v:unknown)=>void)[]=[];
  f.activity=reply=>held.push(reply);
  await f.h.start();await f.select(direct.id);expect(held).toHaveLength(1);
  f.h.sockets[0].close();await drain();await f.advance(1000);
  expect(f.h.sockets).toHaveLength(2);
  f.h.sockets[1].open();await drain();expect(held).toHaveLength(2);
  const painted=f.h.renders.length;
  held[0]([seat(),seat('old/ghost','idle')]);await drain();
  expect(f.h.renders.slice(painted).some(s=>s.activity?.has(lead)||s.seatIds?.includes('old/ghost'))).toBe(false);
  expect(f.marks()).toEqual([]);expect(f.h.state().seatIds).toBeUndefined();
  held[1]([seat(lead,'attention_needed')]);await drain();
  expect(f.marks()).toEqual([[lead,'attention_needed']]);expect(f.h.state().seatIds).toEqual([lead]);
});

it('keeps a server without agent.list live',async()=>{
  const f=activityFixture(false);await f.h.start();await f.select(direct.id);await f.event('msg.delivered');
  expect(f.h.state().phase).toBe('live');expect(f.read()).toHaveLength(0);expect(f.marks()).toEqual([]);
});

it('installs Watch before catch-up and reconnects from the event cursor, filling short root pages', async ()=>{
  const h=fixture(); await h.start();
  expect(h.sent.slice(0,4).map(p=>p.method ?? p.type)).toEqual(['connect','channel.list','watch','ping']);
  expect(h.sent.find(p=>p.type==='watch').cursor).toBe(15);
  expect(h.state().phase).toBe('live');
  let history:any[] = [];
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:30,channels:[{...channel,activity_seq:30}]});
    if(p.method==='decision.list') reply({decisions:[q],next_after:null});
    if(p.method==='channel.history') {
      history.push(p.params);
      if (p.params.after==='m10') reply({seq:30,channel:'demo',posts:[row(30)],more:true});
      if (p.params.before==='m30') reply({seq:30,channel:'demo',posts:[row(29)],more:true});
      if (p.params.before==='m29') reply({seq:30,channel:'demo',posts:[row(10),row(9,{replies:3})],more:false});
      if (p.params.after==='m30') reply({seq:30,channel:'demo',posts:[],more:false});
      if (!p.params.after && !p.params.before) reply({seq:30,channel:'demo',posts:[row(30),row(29),row(10),row(9,{replies:3})],more:false});
    }
  };
  h.event(21); await drain();
  expect(history.map(p=>p.after ?? p.before)).toEqual(['m10','m30','m29']);
  expect(roots(h.state(),key).map(r=>[r.id,r.replies])).toEqual([['m9',3],['m10',0],['m29',0],['m30',0]]);
  const old=h.sockets.at(-1);
  old?.close(); h.client.dispatch({type:'wake'}); h.sockets.at(-1)?.open(); await drain();
  expect(h.sent.filter(p=>p.type==='watch').map(p=>p.cursor)).toEqual([15,21]);
  expect(h.state().phase).toBe('live');
  old?.message(JSON.stringify({type:'event',seq:40,kind:'channel.posted'}));
  expect(h.state().histories.get(key)?.rows.size).toBe(4);
});

const wakeReceipt={post_id:'m11',channel:'demo',thread:null,woke:[{to:lead},{to:'demo/down'}],
  unavailable:[{to:'demo/down'}],unresolved:[{text:'@ghost'}]};
const wakeLine='Woken: demo/lead · Unavailable: demo/down · Unresolved: @ghost';

function receiptFixture() {
  const h=fixture(), held:((value:unknown)=>void)[]=[];
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel,direct]});
    if(p.method==='channel.history') reply({seq:15,channel:p.params.channel,posts:p.params.channel!=='demo'||p.params.after?[]:[row(9)],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.post') held.push(reply);
  };
  const select=(id:string)=>{h.client.dispatch({type:'select',channel:id});return drain();};
  const thread=()=>{h.client.dispatch({type:'thread',id:'m9'});return drain();};
  function submit(key:string) {
    h.client.dispatch({type:'draft',key,text:'A mention',start:9,end:9});
    h.client.dispatch({type:'submit',key});
  }
  return {h,held,select,thread,submit};
}

it('shows feedback only under the sending composer and clears it on the next send or exit',async()=>{
  const f=receiptFixture(), replyKey=scopeKey('demo','m9');
  await f.h.start();await f.select('demo');f.submit(key);
  f.held[0](wakeReceipt);await drain();
  expect(f.h.state().wakeSummary).toEqual({key,text:wakeLine});
  await f.thread();expect(f.h.state().wakeSummary).toBeUndefined();
  f.submit(replyKey);f.held[1]({...wakeReceipt,thread:'m9'});await drain();
  expect(f.h.state().wakeSummary).toEqual({key:replyKey,text:wakeLine});
  f.submit(replyKey);expect(f.h.state().wakeSummary).toBeUndefined();
  f.held[2]({post_id:'m12',channel:'demo',thread:'m9'});await drain();
  expect(f.h.state().wakeSummary).toBeUndefined();expect(f.h.state().attempts.size).toBe(0);
});

it('settles older sends without replacing feedback after a view round trip or a newer submit',async()=>{
  const f=receiptFixture(), replyKey=scopeKey('demo','m9');
  await f.h.start();await f.select('demo');f.submit(key);
  await f.select(direct.id);await f.select('demo');
  f.held[0](wakeReceipt);await drain();
  expect(f.h.state().wakeSummary).toBeUndefined();expect(f.h.state().attempts.has(key)).toBe(false);
  await f.thread();f.submit(key);f.submit(replyKey);
  expect(f.held).toHaveLength(2);
  f.held[1]({...wakeReceipt,post_id:'m12'});await drain();
  expect(f.h.state().wakeSummary).toBeUndefined();
  f.held[2]({...wakeReceipt,post_id:'m13',thread:'m9'});await drain();
  expect(f.h.state().wakeSummary).toEqual({key:replyKey,text:wakeLine});
  expect(f.h.state().attempts.size).toBe(0);expect(f.h.state().drafts.size).toBe(0);
});

it('parks a shared text reply after a downgrade without silently changing its target',async()=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture(), thread=scopeKey('demo','m9');let old=false;
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') {
      if(old&&p.params.include_shared_replies) refuse('invalid_params');
      else reply({seq:15,channel:'demo',posts:p.params.thread||p.params.after?[]:[row(9)],more:false});
    }
    if(p.method==='channel.post') refuse('timeout');
  };
  const write=()=>h.sent.filter(p=>p.method==='channel.post').map(p=>p.params);
  await h.start();expect(h.state().canShare).toBe(true);
  h.client.dispatch({type:'thread',id:'m9'});await drain();
  h.client.dispatch({type:'draft',key:thread,text:'Share this',start:10,end:10,share:true});
  h.client.dispatch({type:'submit',key:thread});await drain();
  const flagged={body:'Share this',client_key:'a-deduplication-key',reply_to:'m9',also_in_chat:true};
  expect(write()).toEqual([flagged]);
  old=true;h.sockets.at(-1)?.close();h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();
  expect(h.state()).toMatchObject({canShare:false,phase:'live'});
  expect(h.state().outbox).toMatchObject([{failed:true,params:flagged}]);
  h.client.dispatch({type:'outbox-retry',id:flagged.client_key});await drain();
  expect(write()).toEqual([flagged]);
  expect(h.sent.filter(p=>p.method==='channel.history'&&p.params.include_shared_replies&&p.params.limit===1)).toHaveLength(2);
});

it('loads the missing root once without moving the main paging cursor',async()=>{
  const h=fixture();let legacy=false;
  const shared=[row(52,{thread:'m2',also_in_chat:true}),row(51,{thread:'m2',also_in_chat:true})];
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:55,channels:[channel]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') {
      const {thread,include_shared_replies,limit,after}=p.params;
      if(legacy&&include_shared_replies) {refuse('invalid_params');return;}
      const posts=thread==='m2' ? [row(2)] : after ? [] :
        include_shared_replies ? (limit===1 ? shared.slice(0,1) : [...shared,row(50)]) : [row(50)];
      reply({seq:55,channel:'demo',posts,more:false});
    }
  };
  await h.start();
  expect(roots(h.state(),key).map(r=>r.id)).toEqual(['m50','m51','m52']);
  expect(h.sent.filter(p=>p.method==='channel.history'&&p.params.thread==='m2'&&p.params.limit===1)).toHaveLength(1);
  h.client.dispatch({type:'thread',id:'m2'});await drain();
  expect(h.state()).toMatchObject({pane:'thread',thread:'m2'});
  legacy=true;const from=h.sent.length;
  h.sockets.at(-1)?.close();h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();
  const main=h.sent.slice(from).filter(p=>p.method==='channel.history'&&!p.params.thread);
  expect(main.find(p=>p.params.after)?.params.after).toBe('m50');
  expect(roots(h.state(),key).map(r=>r.id)).toEqual(['m50']);
});

it('a replay reset forgets even the current link’s already-tried thread heads',()=>{
  // Enter this boundary directly: the current first-Watch guard does not
  // offer a socket-only way to populate and reset the same Link.
  const heads=new Set<`m${bigint}`>(['m2']);
  const s=resetForReplay({...initial(),listSeq:55},heads);
  expect(s.listSeq).toBeNull();expect(heads.has('m2')).toBe(false);
});

it('reuses one in-flight and settled HEAD across attachment row rebuilds, scoped to a link',async()=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const pending=Promise.withResolvers<Response>();
  const storeHeaders={'content-type':'application/octet-stream','cache-control':'private, max-age=31536000, immutable'};
  const fetch=vi.fn<typeof globalThis.fetch>().mockReturnValueOnce(pending.promise)
    .mockResolvedValueOnce(new Response(null,{headers:{...storeHeaders,'content-length':'22'}}));
  vi.stubGlobal('fetch',fetch);
  const path='/store/1/report.pdf', h=fixture();
  let seq=15;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[{...channel,activity_seq:seq}]});
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:p.params.after?[]:
      [row(9,{attachments:[path],replies:seq-15})],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
  };
  const buildChip=(replies:number)=>{
    const post=roots(h.state(),key).at(0)!;
    expect(post.replies).toBe(replies); // A new render signature, but the same attachment path.
    return h.client.fileHead(post.attachments.at(0)!);
  };
  await h.start();h.client.dispatch({type:'select',channel:'demo'});await drain();
  const first=buildChip(0);
  h.event(++seq,'channel.posted');await drain();
  const rebuilt=buildChip(1);
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(fetch).toHaveBeenLastCalledWith('/file?p=%2Fstore%2F1%2Freport.pdf',expect.objectContaining({method:'HEAD'}));
  pending.resolve(new Response(null,{headers:{...storeHeaders,'content-length':'11'}}));
  const stored={size:11,immutable:true};
  await expect(Promise.all([first,rebuilt])).resolves.toEqual([stored,stored]);
  h.event(++seq,'channel.posted');await drain();
  await expect(buildChip(2)).resolves.toEqual(stored);
  expect(fetch).toHaveBeenCalledTimes(1);
  h.sockets.at(-1)?.close();await drain();vi.advanceTimersByTime(1000);
  expect(h.sockets).toHaveLength(2);h.sockets.at(-1)?.open();await drain();
  await expect(buildChip(2)).resolves.toEqual({size:22,immutable:true});
  expect(fetch).toHaveBeenCalledTimes(2);
});

it('shares concurrent report HEADs but reads new metadata after the report changes',async()=>{
  const h=fixture();await h.start();
  const pending=Promise.withResolvers<Response>();
  const headers={'content-type':'text/plain; charset=utf-8','cache-control':'no-store'};
  const fetch=vi.fn<typeof globalThis.fetch>().mockReturnValueOnce(pending.promise)
    .mockResolvedValueOnce(new Response(null,{headers:{...headers,'content-length':'25'}}));
  vi.stubGlobal('fetch',fetch);
  const path='/docs/plans/report.md';
  const first=h.client.fileHead(path), second=h.client.fileHead(path);
  expect(fetch).toHaveBeenCalledTimes(1);
  pending.resolve(new Response(null,{headers:{...headers,'content-length':'14'}}));
  await expect(Promise.all([first,second])).resolves.toEqual([{size:14,text:true},{size:14,text:true}]);
  await expect(h.client.fileHead(path)).resolves.toEqual({size:25,text:true});
  expect(fetch).toHaveBeenCalledTimes(2);
});

it('allows a fresh check after a busy or unavailable file changes',async()=>{
  const h=fixture();await h.start();
  vi.stubGlobal('fetch',vi.fn<typeof fetch>()
    .mockResolvedValueOnce(new Response(null,{status:503}))
    .mockResolvedValueOnce(new Response(null,{status:404}))
    .mockResolvedValueOnce(new Response(null,{headers:{'content-length':'3','content-type':'text/plain','cache-control':'no-store'}})));
  const path='/docs/plans/report.txt';
  await expect(h.client.fileHead(path)).resolves.toEqual({});
  await expect(h.client.fileHead(path)).resolves.toEqual({unavailable:'No longer there'});
  await expect(h.client.fileHead(path)).resolves.toEqual({size:3,text:true});
});

const jpeg = new Blob(['fixture'],{type:'image/jpeg'});
const pick=(h:ReturnType<typeof fixture>,name:string,target=key)=>h.client.dispatch({type:'pick',key:target,name,blob:jpeg,photo:true});
const posts=(h:ReturnType<typeof fixture>)=>h.sent.filter(f=>f.method==='channel.post').map(f=>f.params);

it('locks sending and posts one mixed attachment-only message after both uploads',async()=>{
  const h=fixture();await h.start();
  const pdf=new Blob(['%PDF-test\n'],{type:'application/pdf'});
  pick(h,'one.jpg');h.client.dispatch({type:'pick',key,name:'report.pdf',blob:pdf,photo:false});
  expect(h.state().photos.get(key)?.map(p=>[p.name,p.size,p.photo])).toEqual([['one.jpg',7,true],['report.pdf',10,false]]);
  const ready=Promise.withResolvers<{path:string;size:number}>();
  const upload=vi.fn<SessionIO['upload']>().mockReturnValueOnce(ready.promise).mockResolvedValueOnce({path:'/store/2/report.pdf',size:10});
  h.upload=upload;
  h.frames=(f,reply)=>{if(f.method==='channel.post')reply({channel:'demo',post_id:'m20'});};
  h.client.dispatch({type:'submit',key});h.client.dispatch({type:'submit',key});
  expect(h.state().attempts.get(key)?.phase).toBe('sending');
  expect(posts(h)).toEqual([]);
  ready.resolve({path:'/store/1/one.jpg',size:7});await drain();
  expect(upload.mock.calls).toEqual([[jpeg,'one.jpg',expect.any(AbortSignal)],[pdf,'report.pdf',expect.any(AbortSignal)]]);
  expect(posts(h)).toEqual([{channel:'demo',body:'',client_key:'a-deduplication-key',attachments:['/store/1/one.jpg','/store/2/report.pdf']}]);
  expect(h.state().photos.get(key)).toBeUndefined();
});

it('keeps the draft and its picks when upload fails, without posting',async()=>{
  const h=fixture();await h.start();pick(h,'one.jpg');
  h.upload=async ()=>{throw new Error('Files are not set up.');};
  h.client.dispatch({type:'draft',key,text:'Keep this',start:0,end:9});
  h.client.dispatch({type:'submit',key});await drain();
  expect(posts(h)).toEqual([]);
  expect(h.state().attempts.size).toBe(0);
  expect(h.state().photos.get(key)).toEqual([expect.objectContaining({name:'one.jpg',size:7})]);
  expect(h.state().drafts.get(key)?.text).toBe('Keep this');
  expect(h.state().notices.get(key)).toBe('Files are not set up.');
});

it.each([404,405])('explains that files are not set up when the bridge returns %s',async status=>{
  const fetch=vi.fn<typeof globalThis.fetch>().mockResolvedValue(new Response(null,{status}));
  vi.stubGlobal('fetch',fetch);
  const {signal}=new AbortController();
  await expect(fetchUpload(jpeg,'one.jpg',signal)).rejects.toThrow('Files are not set up.');
  expect(fetch).toHaveBeenCalledWith('/upload?name=one.jpg',expect.objectContaining({method:'POST',body:jpeg,signal}));
});

it('aborts a stalled upload after three minutes and allows another Send or Reload',async()=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  let aborted=false;
  const fetch=vi.fn<typeof globalThis.fetch>().mockImplementationOnce((_url,opts)=>new Promise((_resolve,reject)=>{
    const signal=opts?.signal;
    signal?.addEventListener('abort',()=>{aborted=true;reject(signal.reason);},{once:true});
  })).mockResolvedValueOnce(Response.json({path:'/store/2/one.jpg',size:7},{status:201}));
  vi.stubGlobal('fetch',fetch);
  const h=fixture();await h.start();pick(h,'one.jpg');h.upload=fetchUpload;
  h.client.dispatch({type:'draft',key,text:'Keep this',start:0,end:9});
  h.client.dispatch({type:'submit',key});
  await vi.advanceTimersByTimeAsync(179_999);
  expect(h.state().attempts.get(key)?.phase).toBe('sending');expect(aborted).toBe(false);
  await vi.advanceTimersByTimeAsync(1);
  const state=h.state();
  expect(aborted).toBe(true);expect(posts(h)).toEqual([]);
  expect(state.attempts.size).toBe(0); // Update's Reload gate reads this map.
  expect(state.notices.get(key)).toBe("Couldn't upload this file.");
  expect(state.photos.get(key)?.map(p=>p.name)).toEqual(['one.jpg']);
  expect(state.drafts.get(key)?.text).toBe('Keep this');
  expect(sendControl(true,'Keep this',state.attempts.get(key)?.phase,true).disabled).toBe(false);
  h.frames=(p,reply)=>{if(p.method==='channel.post')reply({channel:'demo',post_id:'m20'});};
  h.client.dispatch({type:'submit',key});await drain();
  expect(posts(h)).toMatchObject([{body:'Keep this',attachments:['/store/2/one.jpg']}]);
});

it('retries a photo reply with the same paths and client key, without re-uploading',async()=>{
  const h=fixture();
  h.frames=(f,reply)=>{
    if(f.method==='channel.list')reply({seq:15,channels:[channel]});
    if(f.method==='decision.list')reply({decisions:[]});
    if(f.method==='channel.history')reply({seq:15,channel:'demo',posts:f.params.thread||f.params.after?[]:[row(9)],more:false});
  };
  await h.start();
  h.client.dispatch({type:'thread',id:'m9'});await drain();
  const thread=scopeKey('demo','m9');pick(h,'thread.jpg',thread);pick(h,'main.jpg');
  const upload=vi.fn<SessionIO['upload']>().mockResolvedValue({path:'/store/3/thread.jpg',size:7});h.upload=upload;
  h.frames=(f,_reply,_drop,refuse)=>{if(f.method==='channel.post')refuse('timeout');};
  h.client.dispatch({type:'draft',key:thread,text:'',start:0,end:0,share:true});
  h.client.dispatch({type:'submit',key:thread});await drain();
  expect(h.state().attempts.get(thread)?.phase).toBe('unknown');
  h.frames=(f,reply)=>{if(f.method==='channel.post')reply({channel:'demo',thread:'m9',post_id:'m20'});};
  h.client.dispatch({type:'submit',key:thread,retry:true});await drain();
  const sent={reply_to:'m9',also_in_chat:true,body:'',client_key:'a-deduplication-key',attachments:['/store/3/thread.jpg']};
  expect(posts(h)).toEqual([sent,sent]);expect(upload.mock.calls).toEqual([[jpeg,'thread.jpg',expect.any(AbortSignal)]]);
  expect(h.state().photos.get(key)?.map(p=>p.name)).toEqual(['main.jpg']);
});

it('reuses the uploaded path and key after a definite post refusal',async()=>{
  const h=fixture();let keys=0;h.uuid=()=>`attempt-${++keys}`;
  await h.start();pick(h,'one.jpg');
  const upload=vi.fn<SessionIO['upload']>().mockResolvedValue({path:'/store/1/one.jpg',size:7});h.upload=upload;
  h.frames=(p,_reply,_drop,refuse)=>{if(p.method==='channel.post')refuse('invalid_params');};
  h.client.dispatch({type:'submit',key});await drain();
  expect(h.state().attempts.size).toBe(0);
  const sent={channel:'demo',body:'',client_key:'attempt-1',attachments:['/store/1/one.jpg']};
  expect(posts(h)).toEqual([sent]);
  h.frames=(p,reply)=>{if(p.method==='channel.post')reply({channel:'demo',post_id:'m20'});};
  h.client.dispatch({type:'submit',key});await drain();
  expect(posts(h)).toEqual([sent,sent]);expect(upload).toHaveBeenCalledTimes(1);
});

it('resumes a partial photo batch without uploading a successful pick again',async()=>{
  const h=fixture();let keys=0;h.uuid=()=>`attempt-${++keys}`;
  await h.start();pick(h,'one.jpg');pick(h,'two.jpg');
  const upload=vi.fn<SessionIO['upload']>().mockResolvedValueOnce({path:'/store/1/one.jpg',size:7})
    .mockRejectedValueOnce(new Error('Upload failed')).mockResolvedValue({path:'/store/2/two.jpg',size:7});
  h.upload=upload;
  h.frames=(p,reply)=>{if(p.method==='channel.post')reply({channel:'demo',post_id:'m20'});};
  h.client.dispatch({type:'submit',key});await drain();expect(posts(h)).toEqual([]);
  h.client.dispatch({type:'submit',key});await drain();
  expect(upload.mock.calls.map(([,name])=>name)).toEqual(['one.jpg','two.jpg','two.jpg']);
  expect(posts(h)).toEqual([{channel:'demo',body:'',client_key:'attempt-1',attachments:['/store/1/one.jpg','/store/2/two.jpg']}]);
});

it('bounds picks to eight per draft and removal frees a slot',async()=>{
  const h=fixture();await h.start();
  for(let i=0;i<9;i++)pick(h,`${i}.jpg`);
  const photos=h.state().photos.get(key)??[];
  expect(photos.map(p=>p.name)).toEqual(Array.from({length:8},(_,i)=>`${i}.jpg`));
  h.client.dispatch({type:'remove-photo',key,id:photos.at(0)!.id});pick(h,'replacement.jpg');
  expect(h.state().photos.get(key)?.map(p=>p.name)).toEqual([...photos.slice(1).map(p=>p.name),'replacement.jpg']);
  h.client.dispatch({type:'draft',key,text:'x'.repeat(16385),start:0,end:0});
  h.client.dispatch({type:'submit',key});await drain();
  expect(posts(h)).toEqual([]);expect(h.state().notices.get(key)).toBe('Message is over 16 KiB.');
});

const manual={type:'refresh',source:'channel'} as const;
it('joins pulls behind one forced read pass without changing unsettled writes',async()=>{
  const h=fixture();let hold=false, blocked:((value:unknown)=>void)|undefined;
  h.frames=(p,reply,_drop,refuse)=>{
    switch(p.method){
      case 'channel.list': reply({seq:15,channels:[channel]});break;
      case 'channel.history':
        if(hold&&!blocked) {blocked=reply;break;}
        reply({seq:15,channel:'demo',posts:p.params.after?[]:[row(10)],more:false});break;
      case 'decision.list': reply({decisions:[q]});break;
      case 'channel.post': case 'decision.resolve': refuse('timeout');break;
    }
  };
  await h.start();h.client.dispatch({type:'select',channel:'demo'});await drain();
  h.client.dispatch({type:'draft',key,text:'Do not replay',start:2,end:6});
  h.client.dispatch({type:'submit',key}); h.client.dispatch(answer('Yes'));await drain();
  const {drafts,attempts,answers,outbox}=h.state(), from=h.sent.length;
  expect(outbox).toMatchObject([{params:{body:'Do not replay',client_key:'a-deduplication-key'}}]);
  expect(answers.get('d8')).toMatchObject({phase:'unknown',action:{id:'d8',option:'Yes'}});
  hold=true;h.client.dispatch(manual);await drain();
  expect(h.state().refreshing).toBe('channel');expect(blocked).toBeTypeOf('function');
  h.client.dispatch(manual);h.client.dispatch(manual);await drain();
  const requests=()=>h.sent.slice(from).map(f=>f.method);
  expect(requests().filter(m=>m==='channel.list')).toHaveLength(1);
  h.event(16);blocked?.({seq:15,channel:'demo',posts:[row(12)],more:false});await drain();
  expect(requests().filter(m=>m==='channel.list')).toHaveLength(2); // The real Watch follow-up is not swallowed.
  expect(requests()).toContain('decision.list');expect(requests()).toContain('channel.history');
  expect(requests().filter(m=>m==='channel.post'||m==='decision.resolve'||m==='decision.withdraw')).toEqual([]);
  expect(roots(h.state(),key).map(r=>r.id)).toContain('m12');
  expect(h.state()).toMatchObject({drafts,attempts,answers,refreshing:undefined});
});

it('does not invalidate an answer preflight just to re-list on pull',async()=>{
  const h=fixture();let hold=false, complete:((value:unknown)=>void)|undefined;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') {if(hold) complete=reply;else reply({decisions:[q]});}
    if(p.method==='decision.resolve') reply({decision:{...q,status:'resolved'},option:'Yes'});
  };
  await h.start();hold=true;h.client.dispatch(answer('Yes'));
  h.client.dispatch(manual);await drain();
  expect(h.state().refreshing).toBe('channel');expect(complete).toBeTypeOf('function');
  hold=false;complete?.({decisions:[q]});await drain();
  expect(h.sent.filter(f=>f.method==='decision.resolve').map(f=>f.params)).toEqual([{id:'d8',option:'Yes'}]);
  expect(h.state().refreshing).toBeUndefined();
});

it('clears a held refresh on going offline without letting its receipt affect the replacement link',async()=>{
  const h=fixture();await h.start();let stale:((value:unknown)=>void)|undefined;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='decision.list') reply({decisions:[q]});
    if(p.method==='channel.history') stale=reply;
  };
  h.client.dispatch(manual);await drain();
  expect(h.state().refreshing).toBe('channel');expect(stale).toBeTypeOf('function');
  h.online=false;h.client.dispatch({type:'wake'});
  const count=h.sent.length;h.client.dispatch(manual);
  stale?.({seq:15,channel:'demo',posts:[row(12)],more:false});await drain();
  expect(h.sent).toHaveLength(count);
  expect(h.state()).toMatchObject({phase:'offline',refreshing:undefined});
  expect(roots(h.state(),key).map(r=>r.id)).toEqual(['m9','m10']);
});

const dmId='demo/lead+owner', dmKey=scopeKey(dmId), repliesKey=scopeKey(dmId,'m9');
const dm={...channel,id:dmId,kind:'direct',section:'lead',members:[{address:'demo/lead'},{address:'owner'}]} as const;
const dmRow=(n:number,extra={})=>row(n,{channel:dmId,...extra});
const dmPosts=(s:ChatState)=>roots(s,repliesKey).map(r=>r.id);

it('navigates to Settings from Home without refreshing the selected conversation',async()=>{
  const navigate=vi.fn(()=>true);
  const h=fixture(undefined,navigate,true);
  await h.start();
  const before=h.sent.length;
  h.client.dispatch({type:'settings'});await drain();
  expect(navigate).toHaveBeenCalledExactlyOnceWith({pane:'settings',channel:'demo'},undefined);
  expect(h.state()).toMatchObject({pane:'settings',channel:'demo'});
  expect(h.sent).toHaveLength(before);
  h.client.dispatch({type:'settings'});
  expect(navigate).toHaveBeenCalledTimes(1);
});

it('does not visit unread posts or start an activity sample when resuming Settings',async()=>{
  const listed={...channel,seen_through:'m9'};
  const seed:ChatState={...initial(),pane:'settings',channel:'demo',channels:[listed as never]};
  const h=fixture(seed,undefined,true);
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[listed]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:p.params.after?[]:[row(10),row(9)],more:false});
    if(p.method==='agent.list') reply([]);
  };
  await h.start();
  expect(h.state()).toMatchObject({phase:'live',pane:'settings'});
  expect(h.state().opening).toBeUndefined();
  expect(h.sent.filter(f=>f.method==='agent.list')).toEqual([]);
});

it('restores a channel draft without completing an abandoned linked-history jump',async()=>{
  const navigations:unknown[]=[];
  const h=fixture(undefined,(view,post)=>{navigations.push({view,post});return true;});
  let older:((value:unknown)=>void)|undefined;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel,dm]});
    if(p.method==='decision.list') reply({decisions:[{...q,channel:dmId}]});
    if(p.method==='channel.history') {
      const {channel:id,before,after}=p.params;
      if(before==='m50'&&id===dmId) {older=reply;return;}
      reply({seq:15,channel:id,posts:after?[]:id===dmId?[dmRow(50)]:[row(10)],more:id===dmId&&!after});
    }
  };
  await h.start();
  h.client.dispatch({type:'select',channel:'demo'});
  h.client.dispatch({type:'draft',key,text:'Saved draft',start:1,end:4});
  h.client.dispatch({type:'open',id:'d8'});await drain();
  expect(h.state().channel).toBe(dmId);
  expect(older).toBeTypeOf('function');
  h.client.dispatch({type:'restore',view:{pane:'channel',channel:'demo'}});
  older?.({seq:15,channel:dmId,posts:[dmRow(9)],more:false});await drain();
  expect(h.state()).toMatchObject({pane:'channel',channel:'demo',focus:undefined});
  expect(h.state().drafts.get(key)).toMatchObject({text:'Saved draft',start:1,end:4});
  expect(navigations).toEqual([
    {view:{pane:'channel',channel:'demo'},post:undefined},
    {view:{pane:'channel',channel:dmId},post:'m9'},
  ]);
});

it('does not restore a thread that is absent from the available channel history',async()=>{
  const h=fixture();await h.start();
  h.client.dispatch({type:'restore',view:{pane:'thread',channel:'unlisted',thread:'m9'}});
  expect(h.state()).toMatchObject({pane:'channel',channel:'demo',thread:undefined});
  h.client.dispatch({type:'restore',view:{pane:'thread',channel:'demo',thread:'m123'}});
  expect(h.state()).toMatchObject({pane:'channel',channel:'demo',thread:undefined});
});

it('hydrates short direct reply pages, older roots and reply-only Watch changes',async()=>{
  const h=fixture();let activity=15;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:activity,channels:[{...dm,activity_seq:activity}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.seen') reply({changed:true,through:p.params.through});
    if(p.method!=='channel.history') return;
    const {thread,before,after}=p.params;
    const send=(posts:unknown[],more=false)=>reply({seq:activity,channel:dmId,posts,more});
    if(thread==='m9') {
      if(after==='m10') send([dmRow(12,{thread})]);
      else if(after==='m12') send([dmRow(16,{thread})]);
      else send([dmRow(10,{thread})],true);
    } else if(thread==='m7') send([dmRow(8,{thread})]);
    else if(before==='m9') send([dmRow(7,{replies:1})]);
    else if(after) send([]);
    else if(before==='m11') send([dmRow(9,{replies:3}),dmRow(7,{replies:1})]);
    else send([dmRow(11),dmRow(9,{replies:activity===15?2:3})],true);
  };
  const requests=()=>h.sent.filter(f=>f.params?.thread).map(f=>[f.params.thread,f.params.after]);
  await h.start();
  expect(requests()).toEqual([['m9','m9'],['m9','m10']]);
  expect(dmPosts(h.state())).toEqual(['m10','m12']);
  h.client.dispatch({type:'older'});await drain();
  expect(roots(h.state(),scopeKey(dmId,'m7')).map(r=>r.id)).toEqual(['m8']);
  activity=16;h.event(16,'channel.posted');await drain();
  expect(dmPosts(h.state())).toEqual(['m10','m12','m16']);
  expect(requests().slice(2)).toEqual([['m7','m7'],['m9','m12']]);
  h.client.dispatch({type:'visible',channel:dmId,through:'m13'});
  h.client.dispatch({type:'visible',channel:dmId,through:'m16'});await drain();
  expect(h.sent.filter(f=>f.method==='channel.seen').map(f=>f.params)).toEqual([{channel:dmId,through:'m16'}]);
});

it('accepts a terminal DM reply page despite the root count, until that count changes',async()=>{
  const h=fixture();let seq=15,replies=3;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[{...dm,activity_seq:seq}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.seen') reply({changed:true,through:p.params.through});
    if(p.method!=='channel.history') return;
    const send=(posts:unknown[],more=false)=>reply({seq,channel:dmId,posts,more});
    const {thread,after}=p.params;
    if(!thread) send(after?[]:[dmRow(11),dmRow(9,{replies})]);
    else if(after==='m9') send([dmRow(10,{thread})],true);
    else if(after==='m10') send([dmRow(12,{thread})]);
    else send(replies===4&&after==='m12'?[dmRow(17,{thread})]:[]);
  };
  const scans=()=>h.sent.filter(f=>f.params?.thread).map(f=>f.params.after);
  const markers=()=>h.sent.filter(f=>f.method==='channel.seen').map(f=>f.params.through);
  await h.start();
  expect(dmPosts(h.state())).toEqual(['m10','m12']);
  expect(scans()).toEqual(['m9','m10']);
  h.client.dispatch({type:'visible',channel:dmId,through:'m12'});await drain();
  expect(markers()).toEqual(['m12']);
  h.event(++seq,'channel.posted');await drain();
  expect(scans()).toEqual(['m9','m10']);
  replies=4;h.event(++seq,'channel.posted');await drain();
  expect(dmPosts(h.state())).toEqual(['m10','m12','m17']);
  h.client.dispatch({type:'visible',channel:dmId,through:'m17'});await drain();
  expect(markers()).toEqual(['m12','m17']);
  expect(scans()).toEqual(['m9','m10','m12']);
});

it('does not advance seen over an incomplete DM or continue a page after switching away',async()=>{
  const h=fixture();let finish:((v:unknown)=>void)|undefined;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel,dm]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.seen') reply({changed:true,through:p.params.through});
    if(p.method==='channel.history') {
      if(p.params.thread) finish=reply;
      else reply({seq:15,channel:p.params.channel,posts:p.params.after ? [] : p.params.channel===dmId ? [dmRow(11),dmRow(9,{replies:2})] : [row(10)],more:false});
    }
  };
  await h.start();h.client.dispatch({type:'select',channel:dmId});await drain();
  expect(finish).toBeTypeOf('function');
  h.client.dispatch({type:'visible',channel:dmId,through:'m11'});
  expect(h.sent.filter(f=>f.method==='channel.seen')).toEqual([]);
  h.client.dispatch({type:'select',channel:'demo'});
  finish?.({seq:15,channel:dmId,posts:[dmRow(10,{thread:'m9'})],more:true});await drain();
  expect(h.state().channel).toBe('demo');
  const scans=()=>h.sent.filter(f=>f.params?.thread==='m9').map(f=>f.params.after);
  expect(scans()).toEqual(['m9']);
  expect(roots(h.state(),key).map(r=>r.id)).toEqual(['m10']);
  h.client.dispatch({type:'select',channel:dmId});await drain();
  expect(scans()).toEqual(['m9','m10']); // more:true was interrupted, not terminal
  finish?.({seq:15,channel:dmId,posts:[],more:false});await drain();
  h.client.dispatch({type:'visible',channel:dmId,through:'m11'});await drain();
  expect(h.sent.filter(f=>f.method==='channel.seen').map(f=>f.params.through)).toEqual(['m11']);
  expect(scans()).toEqual(['m9','m10']);
});

it('does not mark a new DM root before its older roots have refreshed reply counts',async()=>{
  const h=fixture();let activity=15, olderPage:((v:unknown)=>void)|undefined;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:activity,channels:[{...dm,activity_seq:activity}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.seen') reply({changed:true,through:p.params.through});
    if(p.method!=='channel.history') return;
    const send=(posts:unknown[],more=false)=>reply({seq:activity,channel:dmId,posts,more});
    if(p.params.thread==='m9') send([dmRow(16,{thread:'m9'})]);
    else if(p.params.after==='m11') send([dmRow(17)]);
    else if(p.params.before==='m17') olderPage=reply;
    else send([dmRow(11),dmRow(9)]);
  };
  await h.start();activity=17;h.event(17,'channel.posted');await drain();
  expect(roots(h.state(),dmKey).map(r=>r.id)).toEqual(['m9','m11','m17']);
  expect(olderPage).toBeTypeOf('function');
  h.client.dispatch({type:'visible',channel:dmId,through:'m17'});
  expect(h.sent.filter(f=>f.method==='channel.seen')).toEqual([]);
  olderPage?.({seq:17,channel:dmId,posts:[dmRow(11),dmRow(9,{replies:1})],more:false});await drain();
  expect(dmPosts(h.state())).toEqual(['m16']);
  h.client.dispatch({type:'visible',channel:dmId,through:'m17'});await drain();
  expect(h.sent.filter(f=>f.method==='channel.seen').map(f=>f.params)).toEqual([{channel:dmId,through:'m17'}]);
});

it('does not skip missing reply history on reconnect from a cached suffix',async()=>{
  const root={...dmRow(9,{replies:2}),id:'m9' as const};
  const tail={...dmRow(12),id:'m12' as const,thread:'m9' as const};
  const seed=reduce(reduce({...initial(),channel:dmId,pane:'channel'},
    {type:'history',key:dmKey,seq:-1,rows:[root]}),{type:'history',key:repliesKey,seq:-1,rows:[tail]});
  const h=fixture(seed);
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[dm]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') reply({seq:15,channel:dmId,more:false,posts:p.params.thread
      ? (p.params.after==='m12'?[]:[dmRow(10,{thread:'m9'}),tail]) : p.params.after ? [] : [root]});
  };
  await h.start();
  expect(h.sent.find(f=>f.params?.thread==='m9')?.params.after).toBe('m9');
  expect(dmPosts(h.state())).toEqual(['m10','m12']);
});

const queueText=(h:ReturnType<typeof fixture>,text:string,scope=key)=>{
  h.client.dispatch({type:'draft',key:scope,text,start:text.length,end:text.length});
  h.client.dispatch({type:'submit',key:scope});
};

it('clears drafts while writing FIFO per channel, independently of another channel',async()=>{
  const f=receiptFixture();await f.h.start();await f.select('demo');
  queueText(f.h,'one');queueText(f.h,'two');
  expect(f.h.state().drafts.has(key)).toBe(false);
  expect(posts(f.h).map(p=>p.body)).toEqual(['one']);
  await f.select(direct.id);queueText(f.h,'other',scopeKey(direct.id));
  expect(posts(f.h).map(p=>p.body)).toEqual(['one','other']);
  f.held[0]({channel:'demo',post_id:'m16',thread:null});await drain();
  expect(posts(f.h).map(p=>p.body)).toEqual(['one','other','two']);
  expect(new Set(posts(f.h).map(p=>p.client_key)).size).toBe(3);
});

it('drains both channels on the next link if the first handoff throws synchronously',async()=>{
  const params=(id:string)=>({channel:id,client_key:id,body:`To ${id}`});
  const h=fixture({...initial(),outbox:['demo',direct.id].map(id=>({channel:id,params:params(id)}))});
  let cut=false;
  h.beforeSend=f=>{if(f.method==='channel.post'&&!cut) {cut=true;throw new Error('WebSocket closed');}};
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel,direct]});
    if(p.method==='channel.history') reply({seq:15,channel:p.params.channel,posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.post') reply({channel:p.params.channel,post_id:p.params.channel==='demo'?'m16':'m17'});
  };
  await h.start();expect(h.state().phase).toBe('reconnecting');expect(posts(h)).toEqual([]);
  h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();
  expect(h.state().phase).toBe('live');expect(posts(h)).toEqual([params('demo'),params(direct.id)]);
});

it.each([true,false])('keeps the draft and makes no handoff without durable storage or ownership (owned=%s)',async owned=>{
  const h=fixture(undefined,undefined,false,{owned,save:()=>false});await h.start();
  queueText(h,'Keep my draft');await drain();
  expect(h.state().drafts.get(key)?.text).toBe('Keep my draft');
  expect(posts(h)).toEqual([]);
  expect(h.state().notices.has(key)).toBe(true);
});

it('can discard an accepted row when storage stops allowing handoff metadata',async()=>{
  let stored:readonly OutboxItem[]=[];
  const h=fixture(undefined,undefined,false,{save:items=>{
    if(items.some(o=>o.floor!==undefined)) return false;
    stored=items;return true;
  }});
  await h.start();queueText(h,'Keep this recoverable');await drain();
  expect(posts(h)).toEqual([]);expect(h.state().drafts.size).toBe(0);
  expect(stored).toMatchObject([{failed:true,params:{body:'Keep this recoverable'}}]);
  h.client.dispatch({type:'outbox-delete',id:'a-deduplication-key'});
  expect(stored).toEqual([]);expect(h.state().outbox).toEqual([]);
});

it('automatically retries the handed-off head on reconnect without changing key or body', async ()=>{
  const h=fixture(); let committed=0;
  h.frames=(p,reply,drop)=>{
    if (p.method==='channel.list') reply({seq:19,channels:[channel]});
    if (p.method==='channel.history') reply({seq:19,channel:'demo',posts:committed && !p.params.after ? [row(11,{body:' A\nB '})] : [],more:false});
    if (p.method==='decision.list') reply({decisions:[],next_after:null});
    if (p.method==='channel.post') { if (!committed++) drop(); else reply({post_id:'m11',channel:'demo',thread:null}); }
  };
  await h.start();
  h.client.dispatch({type:'draft',key,text:' A\nB ',start:5,end:5});
  h.client.dispatch({type:'submit',key}); await drain();
  expect(posts(h)).toHaveLength(1);
  expect(h.state().drafts.has(key)).toBe(false);
  h.client.dispatch({type:'wake'}); h.sockets.at(-1)?.open(); await drain();
  expect(posts(h)).toEqual(Array.from({length:2},()=>({channel:'demo',body:' A\nB ',client_key:'a-deduplication-key'})));
  expect(h.state().drafts.has(key)).toBe(false);
  expect(roots(h.state(),key).map(r=>r.id)).toEqual(['m11']);
});

it.each(['invalid_params','unknown_channel','not_found','unknown_method','refused'])('parks a %s refusal, lets the next post through, and deletes only locally', async code=>{
  const h=fixture();
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[],next_after:null});
    if(p.method==='channel.post') {
      if(p.params.body==='An editable draft') refuse(code);
      else reply({post_id:'m11',channel:'demo',thread:null});
    }
  };
  await h.start();
  h.client.dispatch({type:'draft',key,text:'An editable draft',start:2,end:5});
  h.client.dispatch({type:'submit',key});await drain();
  expect(h.state().drafts.has(key)).toBe(false);
  expect(h.state().outbox).toMatchObject([{failed:true,params:{body:'An editable draft'}}]);
  queueText(h,'Later message');await drain();
  expect(posts(h).map(p=>p.body)).toEqual(['An editable draft','Later message']);
  const at=h.sent.length;
  h.client.dispatch({type:'outbox-delete',id:'a-deduplication-key'});await drain();
  expect(h.state().outbox.some(o=>o.params.body==='An editable draft')).toBe(false);
  expect(h.sent.slice(at).filter(p=>p.type==='request')).toEqual([]);
});

it.each(['timeout','unavailable','internal','conflict'])('backs off and retains the exact key when a text post gets %s', async code=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture();
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[],next_after:null});
    if(p.method==='channel.post') refuse(code);
  };
  await h.start();h.client.dispatch({type:'draft',key,text:'Keep my key',start:0,end:0});
  h.client.dispatch({type:'submit',key});await drain();
  expect(posts(h)).toHaveLength(1);
  await vi.advanceTimersByTimeAsync(4000);await drain();
  expect(posts(h)).toEqual(Array.from({length:2},()=>({channel:'demo',body:'Keep my key',client_key:'a-deduplication-key'})));
});

it.each(['invalid_params','unknown_method'])('a late keyed commit wins over a %s refusal on replay', async code=>{
  const h=fixture(); let writes=0;
  h.frames=(p,reply,drop,refuse)=>{
    if(p.method==='channel.list') reply(captured.list.result);
    if(p.method==='channel.history') reply({seq:212,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply(captured.decisionLastPage.result);
    if(p.method==='channel.post') {if(++writes===1) drop(); else refuse(code);}
  };
  await h.start(); h.client.dispatch({type:'draft',key,text:'Keep this body',start:1,end:4});
  h.client.dispatch({type:'submit',key}); await drain();
  h.client.dispatch({type:'wake'}); h.sockets.at(-1)?.open(); await drain();
  expect(h.state().outbox).toMatchObject([{failed:true,params:{body:'Keep this body',client_key:'a-deduplication-key'}}]);
  h.event(213,'channel.posted',{author:'other',channel:'demo',thread:null,client_key:'a-deduplication-key'});await drain();
  expect(h.state().outbox.at(0)?.failed).toBe(true);
  h.event(214,'channel.posted',{author:'owner',channel:'another',thread:null,client_key:'a-deduplication-key'});await drain();
  expect(h.state().outbox.at(0)?.failed).toBe(true);
  h.event(215,'channel.posted',{author:'owner',channel:'demo',thread:null,client_key:'a-deduplication-key'});await drain();
  expect(h.state().outbox.find(o=>o.params.client_key==='a-deduplication-key')).not.toMatchObject({failed:true});
  expect(posts(h)).toEqual(Array.from({length:2},()=>({channel:'demo',body:'Keep this body',client_key:'a-deduplication-key'})));
});

it('retires a watch-confirmed head without retry, but waits for its live receipt before a follower',async()=>{
  let stored:readonly OutboxItem[]=[], seq=15;
  const h=fixture(undefined,undefined,false,{save:items=>{stored=items;return true;}});
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[channel]});
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:seq===16&&!p.params.after?[ownerRow(16)]:[],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
  };
  await h.start();queueText(h,'Committed head');queueText(h,'Next in order');
  seq=16;
  h.event(seq,'channel.posted',{author:'owner',channel:'demo',client_key:posts(h).at(0)?.client_key});await drain();
  expect(stored.map(o=>o.params.body)).toEqual(['Next in order']);
  expect(posts(h).map(p=>p.body)).toEqual(['Committed head']);
  h.sockets.at(-1)?.close();h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();
  expect(posts(h).map(p=>p.body)).toEqual(['Committed head','Next in order']);
});

it('recreates a session with the committed head and its queued follower after a lost receipt',async()=>{
  let items:readonly OutboxItem[]=[], seq=15;
  const opts={save:(value:readonly OutboxItem[])=>{items=JSON.parse(JSON.stringify(value));return true;}};
  const h=fixture(undefined,undefined,false,opts);
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[channel]});
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
  };
  await h.start();queueText(h,'The committed one');queueText(h,'Next');
  const [first]=posts(h);expect(items.map(o=>o.params.body)).toEqual(['The committed one','Next']);
  seq=16;h.client.stop();
  const restored=fixture({...initial(),outbox:items},undefined,false,opts);
  restored.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[channel]});
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:p.params.after?[]:[ownerRow(16,{body:'The committed one'})],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.post') reply({channel:'demo',post_id:p.params.body==='Next'?'m17':'m16'});
  };
  await restored.start();
  expect(posts(restored)).toEqual([first,{channel:'demo',body:'Next',client_key:'key-2'}]);
  expect(items).toEqual([]);
  expect(roots(restored.state(),key).map(p=>p.id)).toEqual(['m16']);
});

it('fences lease acquisition with a list sampled after the lease, not a pending older response',async()=>{
  const h=fixture(undefined,undefined,false,{owned:false,save:()=>true});
  let seq=101, hold=false, release:()=>void=()=>{};
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') {
      const result={seq,channels:[channel]};
      if(hold) {hold=false;release=()=>reply(result);} else reply(result);
    }
    if(p.method==='channel.history') reply({seq,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
  };
  await h.start();expect(h.state().listSeq).toBe(101);
  hold=true;h.client.dispatch({type:'refresh',source:'channel'});
  seq=103;
  const params={channel:'demo',body:'From the newer holder',client_key:'other-tab'};
  h.client.dispatch({type:'outbox-open',items:[{channel:'demo',params,floor:102}]});
  expect(h.state().outbox).toMatchObject([{floor:102,params}]);expect(posts(h)).toEqual([]);
  release();await drain();
  expect(h.state().listSeq).toBe(103);expect(posts(h)).toEqual([params]);
  expect(h.state().outbox.at(0)?.failed).toBeUndefined();
});

it.each([16,10])('retires a stored exact shared post on downgrade even at list seq %s',async seq=>{
  const item={channel:'demo',params:{reply_to:'m9' as const,also_in_chat:true as const,body:'Settled reply',client_key:'old-key'},floor:15,id:'m16' as const};
  let stored:readonly OutboxItem[]=[item];
  const h=fixture({...initial(),outbox:stored},undefined,false,{save:items=>{stored=items;return true;}});
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq,channels:[channel]});
    if(p.method==='channel.history') {
      if(p.params.include_shared_replies) refuse('invalid_params');
      else reply({seq,channel:'demo',posts:[row(9)],more:false});
    }
    if(p.method==='decision.list') reply({decisions:[]});
  };
  await h.start();
  expect(h.state()).toMatchObject({canShare:false,phase:'live'});
  expect(stored).toEqual([]);
  expect(h.state().outbox.some(o=>o.failed)).toBe(false);
  expect(posts(h)).toEqual([]);
});

it('parks both restored destinations on an empty list without calling post or probing sharing',async()=>{
  const queued:OutboxItem[]=[
    {channel:'demo',params:{reply_to:'m9',also_in_chat:true,body:'Shared draft',client_key:'first'}},
    {channel:'demo',params:{channel:'demo',body:'Follower',client_key:'next'}},
  ];
  let stored:readonly OutboxItem[]=queued;
  const h=fixture({...initial(),outbox:queued},undefined,false,{save:items=>{stored=items;return true;}});
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:0,channels:[]});
    if(p.method==='decision.list') reply({decisions:[]});
  };
  await h.start();
  expect(h.state().phase).toBe('live');
  expect(stored.map(o=>o.failed)).toEqual([true,true]);
  expect(h.sent.filter(p=>['channel.post','channel.history'].includes(p.method))).toEqual([]);
});

it.each(['home','thread'] as const)('rechecks a restored reply root from %s without blocking a valid follower',async pane=>{
  const seed=reduce({...initial(),listSeq:20,channel:'demo',pane,thread:pane==='thread'?'m9':undefined},
    {type:'history',key,seq:20,rows:[ownerRow(9) as never]});
  const queued:OutboxItem[]=[
    {channel:'demo',params:{reply_to:'m9',body:'Queued thread note',client_key:'first'}},
    {channel:'demo',params:{channel:'demo',body:'Valid follower',client_key:'next'}},
  ];
  let stored:readonly OutboxItem[]=queued;
  const h=fixture({...seed,outbox:queued},undefined,false,{save:items=>{stored=items;return true;}});
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:11,channels:[channel]});
    if(p.method==='channel.history') {
      if(p.params.thread==='m9') refuse('not_found');
      else reply({seq:11,channel:'demo',posts:[],more:false});
    }
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.post') reply({post_id:'m12',channel:'demo',thread:p.params.reply_to??null});
  };
  await h.start();
  expect(h.state().phase).toBe('live');
  expect(posts(h)).toEqual([{channel:'demo',body:'Valid follower',client_key:'next'}]);
  expect(stored).toMatchObject([{failed:true,params:{reply_to:'m9',client_key:'first'}}]);
  expect(stored).toHaveLength(1);
});

it('parks an unconfirmed handoff when the list rolls back below its floor',async()=>{
  const h=fixture({...initial(),outbox:[{channel:'demo',params:{channel:'demo',body:'Old journal',client_key:'old-key'},floor:32}]});
  await h.start();
  expect(h.state().outbox).toMatchObject([{failed:true,params:{body:'Old journal'}}]);
  expect(posts(h)).toEqual([]);
});

it('requeues a refused post behind the next in-flight send with the original key',async()=>{
  const h=fixture(), held:((value:unknown)=>void)[]=[];
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.post') {if(!held.length&&p.params.body==='Try again') refuse('refused');else held.push(reply);}
  };
  await h.start();queueText(h,'Try again');queueText(h,'In flight');await drain();
  const [first,second]=posts(h);
  h.client.dispatch({type:'outbox-retry',id:first.client_key});await drain();
  expect(posts(h)).toEqual([first,second]);
  held.at(0)?.({channel:'demo',post_id:'m16'});await drain();
  expect(posts(h)).toEqual([first,second,first]);
});

it('re-lists a real unlinked decision before sending its exact option', async ()=>{
  const h=fixture(); let done=false;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply(captured.list.result);
    if(p.method==='channel.history') reply({seq:212,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply(done ? {decisions:[],next_after:null} : {...captured.decisionFirstPage.result,next_after:null});
    if(p.method==='decision.resolve') {done=true; reply(captured.resolve.result);}
  };
  await h.start(); const before=h.sent.length;
  h.client.dispatch(answer('First sample','d27')); await drain();
  const actions=h.sent.slice(before).filter(f=>f.method?.startsWith('decision.'));
  expect(actions.slice(0,2).map(f=>f.method)).toEqual(['decision.list','decision.resolve']);
  expect(actions.filter(f=>f.method==='decision.resolve').map(f=>f.params)).toEqual([{id:'d27',option:'First sample'}]);
  expect(h.sent.some(f=>f.method==='channel.post')).toBe(false);
  expect(h.state().decisions).toEqual([]);
});

it('only advances a visible marker and never resolves a decision with a thread post', async ()=>{
  const h=fixture(); let resolved=false;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:p.params.thread || p.params.after ? [] : [row(10),row(9)],more:false});
    if(p.method==='decision.list') reply({decisions:resolved ? [] : [q],next_after:null});
    if(p.method==='channel.post') reply({post_id:'m11',channel:'demo',thread:'m9'});
    if(p.method==='channel.seen') reply({changed:true,through:'m10'});
    if(p.method==='decision.resolve') { resolved=true; reply({changed:true,decision:{...q,status:'resolved',resolution:{option:'Yes'}}}); }
  };
  await h.start(); h.foreground=false;
  const visible={type:'visible',channel:'demo',through:'m10'} as const;
  h.client.dispatch(visible); expect(h.sent.some(p=>p.method==='channel.seen')).toBe(false);
  h.foreground=true;
  h.client.dispatch(visible); h.client.dispatch(visible); await drain();
  h.client.dispatch({...visible,through:'m9'});
  expect(h.sent.filter(p=>p.method==='channel.seen').map(p=>p.params)).toEqual([{channel:'demo',through:'m10'}]);
  expect(h.state().channels.at(0)?.unread).toBe(2); // only list controls the badge
  h.client.dispatch({type:'select',channel:'demo'}); h.client.dispatch({type:'thread',id:'m9'}); await drain();
  const thread=scopeKey('demo','m9');
  h.client.dispatch({type:'draft',key:thread,text:'What about timing?',start:0,end:0});
  h.client.dispatch({type:'submit',key:thread}); await drain();
  expect(h.state().decisions.map(d=>d.id)).toEqual(['d8']);
  expect(h.sent.filter(p=>p.method==='channel.post').map(p=>p.params)).toEqual([{reply_to:'m9',body:'What about timing?',client_key:'a-deduplication-key'}]);
  h.client.dispatch(answer('Yes')); h.client.dispatch(answer('No')); await drain();
  expect(h.sent.filter(p=>p.method==='decision.resolve').map(p=>p.params)).toEqual([{id:'d8',option:'Yes'}]);
  expect(h.state().decisions).toEqual([]);
});

it('discards an invalidated short decision scan before making the next committed view', async ()=>{
  const h=fixture(); let finish:(v:unknown)=>void = ()=>{}; let reads=0;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') {
      if (++reads === 1) reply({decisions:[q],next_after:'d8'});
      else if (reads === 2) finish=reply;
      else reply({decisions:[],next_after:null});
    }
  };
  await h.start();
  expect(h.sent.filter(p=>p.method==='decision.list').at(-1).params.after).toBe('d8');
  h.event(17,'decision.resolved'); finish({decisions:[],next_after:null}); await drain();
  expect(h.state().phase).toBe('live');
  expect(h.state().decisions).toEqual([]);
  expect(reads).toBe(3);
});

it('does not retry an ambiguous answer automatically or allow it to change option', async ()=>{
  const h=fixture(); let attempts=0, resolved=false;
  h.frames=(p,reply,drop)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:resolved ? [] : [q],next_after:null});
    if(p.method==='decision.resolve') { if (++attempts===1) drop(); else { resolved=true; reply({decision:{...q,status:'resolved',resolution:{option:'Yes'}},changed:false}); } }
  };
  await h.start(); h.client.dispatch(answer('Yes')); await drain();
  h.client.dispatch({type:'wake'}); h.sockets.at(-1)?.open(); await drain();
  h.client.dispatch(answer('No')); await drain();
  expect(attempts).toBe(1);
  h.client.dispatch(answer('Yes')); await drain();
  expect(h.sent.filter(p=>p.method==='decision.resolve').map(p=>p.params.option)).toEqual(['Yes','Yes']);
});

it('pings at 60s and ignores an old diagnostic after wake creates a new socket', async ()=>{
  vi.useFakeTimers({toFake:['setTimeout','clearTimeout']});
  const h=fixture(); let finish:(p:'daemon-down')=>void=()=>{};
  h.probe=()=>new Promise(r=>{finish=r;}); await h.start();
  vi.advanceTimersByTime(59_999); expect(h.sent.filter(p=>p.type==='ping')).toHaveLength(1);
  vi.advanceTimersByTime(1); await drain(); expect(h.sent.filter(p=>p.type==='ping')).toHaveLength(2);
  h.sockets.at(-1)?.close(); h.client.dispatch({type:'wake'}); h.client.dispatch({type:'wake'});
  h.sockets.at(-1)?.open(); await drain(); finish('daemon-down'); await drain();
  expect(h.sockets.length).toBe(2); expect(h.state().phase).toBe('live');
});

it('does not go Live on a watch refresh until the foreground resume Pong arrives', async()=>{
  const h=fixture(); await h.start();
  let finish:()=>void=()=>{}; h.pong=send=>{finish=send;};
  h.client.dispatch({type:'wake'});h.event(16);await drain();
  expect(h.state().phase).toBe('reconnecting');
  finish();await drain();expect(h.state().phase).toBe('live');
});

it('explains an invalidated answer check without sending a stale option', async()=>{
  const h=fixture();let hold=false,finish:(v:unknown)=>void=()=>{};
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') {if(hold){hold=false;finish=reply;}else reply({decisions:[q],next_after:null});}
  };
  await h.start();hold=true;
  h.client.dispatch(answer('Yes'));h.event(17,'decision.taken_provisionally');
  finish({decisions:[q],next_after:null});await drain();
  expect(h.state().answers.has('d8')).toBe(false);
  expect(h.sent.filter(p=>p.method==='decision.resolve')).toHaveLength(0);
  expect(h.state().notices.get('d8')).toBe("Couldn't confirm that answer.");
});

const taken={...q,status:'taken_provisionally',provisional:{option:'Yes'}};
const withdraw={id:'d8',reason:'No longer needed'} as const;
const overrule={id:'d8',option:'No',pick:'Yes'} as const;
const writes=(h:ReturnType<typeof fixture>)=>h.sent.filter(p=>['decision.resolve','decision.withdraw'].includes(p.method));
const sendDecision=(h:ReturnType<typeof fixture>,action:DecisionAction)=>h.client.dispatch({type:'decision',action});

it('sends one exact overrule across cards and refuses a second concurrent write',async()=>{
  const h=fixture(), other={...q,id:'d9'}; let open=[taken,other], finish:(v:unknown)=>void=()=>{};
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:open});
    if(p.method==='decision.resolve') finish=reply;
  };
  await h.start(); sendDecision(h,overrule); h.client.dispatch(answer('Yes','d9')); await drain();
  expect(writes(h).map(f=>[f.method,f.params])).toEqual([['decision.resolve',{id:'d8',option:'No'}]]);
  open=[other]; finish({changed:true,decision:{...taken,status:'overruled',resolution:{option:'No'}}});await drain();
  expect(h.state().decisions.map(d=>d.id)).toEqual(['d9']); expect(h.state().answers.size).toBe(0);
});

it.each([['answer',answer('No').action],['withdrawal',withdraw]] as const)('does not submit a %s after the list changes a pending row to provisional',async (_kind,action)=>{
  const h=fixture();let current=q;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[current]});
  };
  await h.start(); current=taken; sendDecision(h,action);await drain();
  expect(writes(h)).toHaveLength(0); expect(h.state().decisions[0]?.status).toBe('taken_provisionally');
  expect(h.state().notices.get('d8')).toMatch(/^Couldn't confirm/); expect(h.state().answers.size).toBe(0);
});

it('repeats only the exact uncertain withdrawal and clears it on a matching receipt', async()=>{
  const h=fixture();let attempts=0,open=true;
  h.frames=(p,reply,drop)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:open?[q]:[]});
    if(p.method==='decision.withdraw') {
      if(++attempts===1) drop(); else {open=false;reply({changed:true,decision:{...q,status:'withdrawn'}});}
    }
  };
  await h.start();sendDecision(h,withdraw);await drain();
  h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();
  h.client.dispatch(answer('No'));sendDecision(h,{...withdraw,reason:'Different'});await drain();
  expect(writes(h).map(f=>f.params)).toEqual([{id:'d8',reason:'No longer needed'}]);
  sendDecision(h,withdraw);await drain();
  expect(writes(h).map(f=>[f.method,f.params])).toEqual(Array(2).fill(['decision.withdraw',{id:'d8',reason:'No longer needed'}]));
  expect(h.state().decisions).toEqual([]);expect(h.state().answers.size).toBe(0);
});

it.each([{id:'d9',status:'overruled',option:'No'},{id:'d8',status:'resolved',option:'No'},
  {id:'d8',status:'overruled',option:'Yes'}])('keeps an open card when its overrule receipt is $id/$status/$option',async receipt=>{
  const h=fixture();
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:[taken]});
    if(p.method==='decision.resolve') reply({changed:true,decision:{...taken,id:receipt.id,status:receipt.status,resolution:{option:receipt.option}}});
  };
  await h.start(); sendDecision(h,overrule);await drain();
  expect(h.state().decisions.map(r=>r.id)).toEqual(['d8']);
  expect(h.state().answers.get('d8')).toEqual({phase:'unknown',action:overrule});
});

it.each(['reconnecting','identity'] as const)('drops held decision state on %s when canonical row is gone',async phase=>{
  const h=fixture();let open=true, finish:(v:unknown)=>void=()=>{};
  h.probe=async()=>phase;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:open?[q]:[]});
    if(p.method==='decision.withdraw') finish=reply;
  };
  await h.start();sendDecision(h,{id:'d8'});await drain();
  expect(writes(h).map(f=>f.params)).toEqual([{id:'d8'}]);
  const old=h.sockets.at(-1); old?.close();open=false;await drain();
  if(phase!=='identity') { h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain(); }
  finish({changed:true,decision:{...q,status:'withdrawn'}});await drain();
  expect(h.state().answers.size).toBe(0);expect(h.state().decisions).toEqual([]);expect(writes(h)).toHaveLength(1);
  if(phase==='identity') expect(h.state().notices.has('d8')).toBe(false);
});

const keep=(id:`d${bigint}`,option='Yes')=>({id,option,pick:option} as const);
const batch=(...ids:`d${bigint}`[])=>({type:'keep-all',picks:ids.map(id=>keep(id))} as const);

it('keeps all serially, preserving the refusal and excluding new and pending rows',async()=>{
  const h=fixture(), one=taken,two={...taken,id:'d9'},three={...taken,id:'d10'},late={...taken,id:'d11'},pending={...q,id:'d12'};
  let open=[one,two,three,pending], finish=()=>{};
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:open});
    if(p.method==='decision.resolve') {
      const close=(r:typeof taken)=>{open=open.filter(x=>x.id!==r.id);reply({changed:true,decision:{...r,status:'resolved',resolution:{option:'Yes'}}});};
      if(p.params.id==='d8') finish=()=>close(one);
      if(p.params.id==='d9') refuse();
      if(p.params.id==='d10') close(three);
    }
  };
  await h.start();h.client.dispatch(batch('d8','d9','d10'));await drain();
  expect(writes(h).map(f=>f.params)).toEqual([{id:'d8',option:'Yes'}]);
  expect(h.state().keepAll).toEqual({total:3,checked:0,kept:0,running:true});
  h.client.dispatch(answer('No','d12'));open=[...open,late];h.event(17,'decision.taken_provisionally');await drain();
  finish();await drain();
  expect(writes(h).map(f=>f.params)).toEqual([{id:'d8',option:'Yes'},{id:'d9',option:'Yes'},{id:'d10',option:'Yes'}]);
  expect(h.state().keepAll).toEqual({total:3,checked:3,kept:2,running:false});
  expect(h.state().notices.get('d9')).toBe("Couldn't confirm that answer.");
  expect(h.state().decisions.map(d=>d.id)).toEqual(['d9','d12','d11']);
});

it.each(['reconnecting','identity'] as const)('does not resume a batch after %s and never uses a changed pick',async phase=>{
  const h=fixture(), two={...taken,id:'d9'},three={...taken,id:'d10'},four={...taken,id:'d11'};
  let rows=[taken,two,three,four];h.probe=async()=>phase;
  h.frames=(p,reply,drop)=>{
    if(p.method==='channel.list') reply({seq:15,channels:[channel]});
    if(p.method==='channel.history') reply({seq:15,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') reply({decisions:rows});
    if(p.method==='decision.resolve') {
      if(p.params.id==='d8') {
        rows=[{...two,provisional:{option:'No'}},three,four];
        reply({changed:true,decision:{...taken,status:'resolved',resolution:{option:'Yes'}}});
      }
      if(p.params.id==='d10') drop();
    }
  };
  await h.start();h.client.dispatch(batch('d8','d9','d10','d11'));await drain();
  if(phase!=='identity') {h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();}
  expect(writes(h).map(f=>f.params)).toEqual([{id:'d8',option:'Yes'},{id:'d10',option:'Yes'}]);
  if(phase==='identity') {
    expect(h.state().keepAll).toBeUndefined();expect(h.state().answers.size).toBe(0);expect(h.state().notices.size).toBe(0);
  } else for(const id of ['d9','d10','d11']) expect(h.state().notices.get(id)).toBe("Couldn't confirm that answer.");
});

it('keeps the original unknown answer when Retry is invalidated before sending', async()=>{
  const h=fixture(); let hold=false, finish:(v:unknown)=>void=()=>{};
  h.frames=(p,reply,drop)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[],more:false});
    if(p.method==='decision.list') {if(hold){hold=false;finish=reply;}else reply({decisions:[q],next_after:null});}
    if(p.method==='decision.resolve') drop();
  };
  await h.start();h.client.dispatch(answer('Yes'));await drain();
  h.client.dispatch({type:'wake'});h.sockets.at(-1)?.open();await drain();
  hold=true;h.client.dispatch(answer('Yes'));h.event(17,'decision.requested');
  finish({decisions:[q],next_after:null});await drain();
  expect(h.state().answers.get('d8')).toEqual({action:answer('Yes').action,phase:'unknown'});
  h.client.dispatch(answer('No'));await drain();
  expect(h.sent.filter(p=>p.method==='decision.resolve').map(p=>p.params.option)).toEqual(['Yes']);
});

it('does not issue the same refused seen marker on each refresh', async()=>{
  const h=fixture();
  h.frames=(p,reply,_drop,refuse)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:[row(10)],more:false});
    if(p.method==='decision.list') reply({decisions:[],next_after:null});
    if(p.method==='channel.seen') refuse();
  };
  await h.start(); const visible={type:'visible',channel:'demo',through:'m10'} as const;
  h.client.dispatch(visible);await drain();h.client.dispatch(visible);await drain();
  expect(h.sent.filter(p=>p.method==='channel.seen')).toHaveLength(1);
  expect(h.state().channels[0]?.unread).toBe(2);
});

it('freezes the unread boundary at opening and resamples only on a new visit',async()=>{
  const h=fixture(); let seq=20, seen='m9', unread=2;
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[{...channel,unread,seen_through:seen}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') {
      const {after,thread}=p.params;
      const posts=thread?[row(12,{thread:'m9'})]:[row(11),row(10),row(9,{replies:1})];
      reply({seq,channel:'demo',posts:posts.filter(r=>!after||Number(r.id.slice(1))>Number(after.slice(1))).slice(0,p.params.limit),more:false});
    }
  };
  await h.start();
  expect(h.state().opening).toBeUndefined(); // Home can preview a channel without opening it.
  const select=async()=>{h.client.dispatch({type:'select',channel:'demo'});await drain();};
  const home=()=>h.client.dispatch({type:'restore',view:{pane:'home',channel:'demo'}});
  await select();
  expect(h.state().opening).toEqual({after:'m9',target:'m10'});
  const opened=h.state().opening;
  seen='m10'; unread=0;h.event(++seq,'channel.seen');await drain();
  expect(h.state().opening).toBe(opened);
  home();expect(h.state().opening).toBeUndefined();await select();expect(h.state().opening).toBeUndefined();
  unread=2;h.event(++seq,'channel.posted');await drain();expect(h.state().opening).toBeUndefined();
  h.client.dispatch({type:'thread',id:'m9'});await drain();
  expect(h.state().opening).toEqual({after:'m10',target:'m12'});
  h.client.dispatch({type:'restore',view:{pane:'channel',channel:'demo'}});await drain();
  expect(h.state().opening).toEqual({after:'m10',target:'m11'});
  expect(resetForReplay(h.state()).opening).toBeUndefined();
  home();seen='';h.event(++seq);await drain();await select();expect(h.state().opening).toBeUndefined();
});

it.each([false,true])('resolves a paged unread boundary only while that visit is current (left: %s)',async left=>{
  const h=fixture(); let release:(()=>void)|undefined;
  const suffix=Array.from({length:50},(_,i)=>row(60-i));
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:70,channels:[{...channel,activity_seq:70,seen_through:'m9',unread:51}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') {
      const {before,after,limit}=p.params;
      if(before==='m11') release=()=>reply({seq:70,channel:'demo',posts:[row(10),row(9)],more:false});
      else reply({seq:70,channel:'demo',posts:after?[]:suffix.slice(0,limit),more:!after});
    }
  };
  await h.start(); h.client.dispatch({type:'select',channel:'demo'});await drain();
  expect(roots(h.state(),key).at(0)?.id).toBe('m11');
  expect(h.state().opening).toEqual({after:'m9'});
  expect(release).toBeTypeOf('function');
  if(left) h.client.dispatch({type:'restore',view:{pane:'home',channel:'demo'}});
  release?.(); await drain();
  expect(h.state().opening).toEqual(left?undefined:{after:'m9',target:'m10'});
});

it.each([
  {last:160,bounds:['m111','m61','m11'],target:'m10'},
  {last:430,bounds:['m381','m331','m281'],target:undefined},
])('limits automatic unread paging to three older pages ($last posts)',async({last,bounds,target})=>{
  const h=fixture();let seq=500;
  const rows=Array.from({length:last-8},(_,i)=>row(last-i));
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[{...channel,unread:last-9,seen_through:'m9'}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') {
      const {before,after,limit}=p.params;
      const matching=rows.filter(r=>!after&&(!before||Number(r.id.slice(1))<Number(before.slice(1))));
      reply({seq,channel:'demo',posts:matching.slice(0,limit),more:matching.length>limit});
    }
  };
  const older=()=>h.sent.filter(f=>f.method==='channel.history'&&f.params.before).map(f=>f.params.before);
  await h.start();h.client.dispatch({type:'select',channel:'demo'});await drain();
  expect(older()).toEqual(bounds);
  expect(h.state().opening?.target).toBe(target);
  h.event(++seq,'channel.seen');await drain();
  expect(older()).toEqual(bounds); // A relist cannot spend the budget again.
  if(!target) {
    expect(h.state().histories.get(key)?.older).toBe(true);
    h.client.dispatch({type:'older'});await drain();
    expect(older()).toEqual([...bounds,'m231']);
    expect(h.state().opening?.target).toBeUndefined();
  }
});

it.each([0,2])('uses the first live unread count (%s) when resuming a read cached channel',async unread=>{
  const cached={...channel,kind:'project',section:'channel',unread:0,seen_through:'m9'} as const;
  const seed=reduce({...initial(),pane:'channel',channel:'demo'},{type:'channels',seq:15,rows:[cached]});
  const h=fixture(seed);let seq=16, liveUnread=unread, through='m9';
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq,channels:[{...cached,unread:liveUnread,seen_through:through}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') reply({seq,channel:'demo',more:false,
      posts:p.params.after?[]:[row(11),row(10),row(9)].slice(0,p.params.limit)});
  };
  await h.start();
  expect(h.renders.at(0)?.opening).toBeUndefined();
  const opened=h.state().opening;
  expect(opened).toEqual(unread?{after:'m9',target:'m10'}:undefined);
  liveUnread=1;through='m10';h.event(++seq,'channel.seen');await drain();
  expect(h.state().opening).toBe(opened);
});

it('finds New before a cached thread suffix when resuming an unread thread',async()=>{
  const listed={...channel,unread:3,seen_through:'m9' as const} as ChatState['channels'][number];
  const head={...row(8,{replies:4}),id:'m8' as const};
  const replyRows=[row(9,{thread:'m8'}),row(10,{thread:'m8'}),row(11,{thread:'m8'}),row(12,{thread:'m8'})];
  const seed=reduce({...initial(),pane:'thread',channel:'demo',thread:'m8'}, {type:'channels',seq:15,rows:[listed]});
  const main=reduce(seed,{type:'history',key,seq:-1,rows:[head]});
  const h=fixture(reduce(main,{type:'history',key:scopeKey('demo','m8'),seq:-1,rows:[{...row(12),id:'m12',thread:'m8'}]}));
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[{...listed,activity_seq:16}]});
    if(p.method==='decision.list') reply({decisions:[]});
    if(p.method==='channel.history') {
      const {after,thread}=p.params;
      const rows=thread?replyRows:after?[]:[head];
      reply({seq:16,channel:'demo',more:false,posts:rows.filter(r=>!after||Number(r.id.slice(1))>Number(after.slice(1)))});
    }
  };
  await h.start();
  expect(h.state().opening).toEqual({after:'m9',target:'m10'});
});

it('ends older paging when the terminal page has no rows', async()=>{
  const h=fixture();
  h.frames=(p,reply)=>{
    if(p.method==='channel.list') reply({seq:16,channels:[channel]});
    if(p.method==='channel.history') reply({seq:16,channel:'demo',posts:p.params.before?[]:[row(10)],more:!p.params.before});
    if(p.method==='decision.list') reply({decisions:[],next_after:null});
  };
  await h.start();expect(h.state().histories.get(key)?.older).toBe(true);
  h.client.dispatch({type:'older'});await drain();expect(h.state().histories.get(key)?.older).toBe(false);
});

it('only diagnoses daemon or Origin on evidence from same-origin HTTP', async ()=>{
  const signal = new AbortController().signal;
  const call=(routes:Record<string,Response>)=>diagnose(async (url,opts)=>{
    expect(opts?.mode).toBe('same-origin');
    expect(opts?.cache).toBe('no-store');
    if(url==='/ws') expect(opts?.method).toBe('POST');
    return routes[String(url)] ?? new Response(null,{status:405});
  },signal);
  expect(await call({'/health':Response.json({bridge:'ok',daemon:'down',protocol:'1.1'})})).toBe('daemon-down');
  expect(await call({'/health':new Response(null,{status:404})})).toBe('reconnecting');
  expect(await call({'/health':new Response(null,{status:404}),'/ws':Response.json({error:'origin'},{status:403})})).toBe('origin');
  expect(await call({'/health':Response.json({error:'identity'},{status:403})})).toBe('identity');
  expect(await diagnose(async()=>{throw new TypeError('opaque failure');},signal)).toBe('unreachable');
});

import { expect, it } from 'vitest';
import { clearDisplay, readDisplay, saveResume, seedDisplay, snapshot, takeResume, writeDisplay } from '../src/cache.ts';
import { initial, reduce, roots, scopeKey } from '../src/state.ts';
import * as cache from '../src/cache.ts';

const channel={id:'demo',kind:'project',section:'channel',members:[{address:'owner'}],former:[],writable:true,activity_seq:19,unread:3};
const post=(id:number,body='A projected post')=>({id:`m${id}`,channel:'demo',kind:'note',author:'demo/lead',posted_at:'2026-01-01T00:00:00Z',replies:0,mentions:[],attachments:['/fixture/private/status.md'],body});
const display=(posts=[post(9)])=>({seq:19,channels:[channel],decisions:[],roots:posts,replies:[],channel:'demo',pane:'channel',older:false});
function storage() {
  let contents:string|null=null;
  return {getItem(){return contents;},setItem(_k:string,v:string){contents=v;},removeItem(){contents=null;}};
}
it('hydrates only a validated nonauthoritative display projection',()=>{
  const disk=storage();
  writeDisplay(disk,{...display(),drafts:[['in-flight','private unsent']],error:'private diagnostic'} as never);
  expect(disk.getItem()).not.toMatch(/private|diagnostic|unsent|\/fixture/);
  const s=seedDisplay(initial(false),readDisplay(disk));
  expect(s.phase).toBe('offline'); expect(s.channel).toBe('demo'); expect(s.attempts.size).toBe(0);
  expect(roots(s,scopeKey('demo')).map(r=>[r.id,r.attachments])).toEqual([['m9',['status.md']]]);
  clearDisplay(disk);expect(readDisplay(disk)).toBeNull();
});
it('treats a legacy cached pane as display data, not as a navigation entry',()=>{
  const disk=storage();
  disk.setItem('unused',JSON.stringify({...display(),pane:'channel'}));
  const offline=seedDisplay(initial(false),readDisplay(disk));
  expect(offline).toMatchObject({channel:'demo',pane:'home'});
  expect(roots(offline,scopeKey('demo')).map(r=>r.id)).toEqual(['m9']);
});
it('keeps edited copy and removed rows as offline display data, never capabilities',()=>{
  const disk=storage();
  writeDisplay(disk,{...display([{...post(9,'Edited copy'),edited:true},{...post(10,'removed'),removed:true,attachments:[]}]),
    postMethods:['channel.edit','channel.remove']} as never);
  const offline=seedDisplay(initial(false),readDisplay(disk));
  expect(roots(offline,scopeKey('demo'))).toMatchObject([{body:'Edited copy',edited:true},{body:'removed',removed:true,attachments:[]}]);
  expect(offline.postMethods).toBeUndefined();
  expect(disk.getItem()).not.toContain('channel.edit');
});
it('stores reaction counts as display data without persisting actors, capabilities, or locks',()=>{
  const disk=storage();
  writeDisplay(disk,{...display([{...post(9),reactions:[{emoji:'✅',count:2,mine:true,actor:'unused seat'}]}]),
    postMethods:['channel.react'],mutating:[['m9',true]]} as never);
  const offline=seedDisplay(initial(false),readDisplay(disk));
  expect(roots(offline,scopeKey('demo'))[0]?.reactions).toEqual([{emoji:'✅',count:2,mine:true}]);
  expect(disk.getItem()).not.toMatch(/unused seat|channel.react|mutating/);
});
it('does not store an uncertain decision withdrawal or its reason',()=>{
  const disk=storage(), resume=storage();
  const s=reduce(initial(),{type:'patch',value:{answers:new Map([['d8',{
    phase:'unknown',action:{id:'d8',reason:'private fixture reason'},
  }]])}});
  writeDisplay(disk,snapshot(s)); expect(saveResume(resume,s,[])).toBe(true);
  expect([disk.getItem(),resume.getItem()].join()).not.toMatch(/private fixture reason|answers|withdraw/);
  expect(takeResume(resume,seedDisplay(initial(),readDisplay(disk))).s.answers.size).toBe(0);
});

it('resumes the thread delivery choice with the scoped draft and caret',()=>{
  const disk=storage(), key=scopeKey('demo','m9');
  const draft={text:'Resume this draft',revision:2,start:1,end:4,share:true as const};
  const seed={...initial(),drafts:new Map([[key,draft]])};
  expect(saveResume(disk,seed,[])).toBe(true);
  expect(takeResume(disk,initial()).s.drafts.get(key)).toEqual({...draft,revision:0});
});

it('caps display size while retaining newer usable rows',()=>{
  const disk=storage(), posts=Array.from({length:30},(_,i)=>post(i+1,'é'.repeat(8100)));
  writeDisplay(disk,display(posts) as never);
  expect(new TextEncoder().encode(disk.getItem()??'').length).toBeLessThanOrEqual(256*1024);
  const projection=readDisplay(disk);
  expect(projection?.roots.at(-1)?.id).toBe('m30');
  expect(projection?.roots.length).toBeLessThan(30);
  expect(projection?.older).toBe(true);
});
it('round-trips a bounded offline DM without foreign or orphan replies',()=>{
  const id='demo/lead+owner', disk=storage();
  const root=(n:number)=>({...post(n),id:`m${BigInt(n)}` as const,channel:id});
  const reply=(n:number,thread:`m${bigint}`)=>({...root(n),thread});
  const seed={...initial(),channel:id,pane:'channel' as const};
  const channels=[{...channel,id,kind:'direct' as const,section:'lead' as const}];
  let s=reduce(seed,{type:'channels',seq:50,rows:channels});
  s=reduce(s,{type:'history',key:scopeKey(id),seq:50,rows:Array.from({length:31},(_,i)=>root(i+1))});
  s=reduce(s,{type:'history',key:scopeKey(id,'m1'),seq:50,rows:[reply(40,'m1')]});
  s=reduce(s,{type:'history',key:scopeKey(id,'m31'),seq:50,rows:Array.from({length:31},(_,i)=>reply(i+41,'m31'))});
  const projected=snapshot(s);
  writeDisplay(disk,{...projected,replies:[...projected.replies,reply(72,'m999'),{...reply(73,'m31'),channel:'foreign'}]});
  const view=readDisplay(disk);
  expect(view?.roots.map(r=>r.id)).toEqual(Array.from({length:30},(_,i)=>`m${i+2}`));
  expect(view?.replies.map(r=>r.id)).toEqual(Array.from({length:30},(_,i)=>`m${i+42}`));
  expect(disk.getItem()).not.toMatch(/m999|foreign|"id":"m40"/);
  const offline=seedDisplay(initial(false),view);
  expect(offline.histories.get(scopeKey(id,'m31'))?.rows.get('m71')?.seq).toBe(-1);
  expect(offline.histories.get(scopeKey(id))?.older).toBe(true);
  clearDisplay(disk);expect(readDisplay(disk)).toBeNull();
});

function outboxStorage() {
  const values=new Map<string,string>();
  return {getItem:(k:string)=>values.get(k)??null,setItem:(k:string,v:string)=>{values.set(k,v);},removeItem:(k:string)=>{values.delete(k);}};
}
it('round-trips the text-only outbox tuple, rejects malformed flags, and enforces 64 items',()=>{
  const disk=outboxStorage(), key=cache.outboxKey;
  const item={channel:'demo',params:{reply_to:'m9' as const,also_in_chat:true as const,body:' two\nlines ',client_key:'fixture-key'},floor:15,id:'m16' as const};
  expect(cache.writeOutbox(disk,[{...item,params:{...item.params,attachments:['/fixture/file']}} as never])).toBe(true);
  const tuple=['demo','fixture-key',' two\nlines ','m9',true,15,'m16',null];
  expect(JSON.parse(disk.getItem(key)??'null')).toEqual([tuple]);
  expect(cache.readOutbox(disk)).toEqual([{...item,done:true}]);
  const many=Array.from({length:65},(_,i)=>({...item,params:{...item.params,client_key:`key-${i}`}}));
  expect(cache.writeOutbox(disk,many.slice(0,64))).toBe(true);
  expect(cache.writeOutbox(disk,many)).toBe(false);
  expect(cache.readOutbox(disk)).toHaveLength(64);
  for(const bad of [[...tuple.slice(0,4),false,...tuple.slice(5)],[...tuple.slice(0,7),'not sent']]) {
    disk.setItem(key,JSON.stringify([bad]));expect(cache.readOutbox(disk)).toEqual([]);
  }
});

it('reads a stored exact ID as sent even before another tab grants ownership',()=>{
  const disk=outboxStorage();
  disk.setItem(cache.outboxKey,JSON.stringify([['demo','confirmed','Sent note','m9',true,15,'m16',true]]));
  expect(cache.readOutbox(disk)).toMatchObject([{id:'m16',done:true}]);
  expect(cache.readOutbox(disk).at(0)?.failed).toBeUndefined();
});

it('reserves bytes at enqueue for the largest floor and post ID at the 128 KiB boundary',()=>{
  const disk=outboxStorage(), end=131072;
  const floor=9_007_199_254_740_991, id='m18446744073709551615';
  const items=Array.from({length:8},(_,i)=>({channel:'demo',params:{channel:'demo',client_key:`cap-${i}`,body:'x'.repeat(i<7?16384:1)}}));
  const rows=items.map(o=>[o.channel,o.params.client_key,o.params.body,null,null,floor,id,true]);
  const last=items[7].params;
  last.body='x'.repeat(1+end-JSON.stringify(rows).length);
  expect(cache.writeOutbox(disk,items)).toBe(true);
  expect(cache.writeOutbox(disk,items.map(o=>({...o,floor,id,failed:true})))).toBe(true);
  expect(disk.getItem(cache.outboxKey)).toHaveLength(end);
  last.body+='x';
  expect(cache.writeOutbox(disk,items)).toBe(false);
  expect(disk.getItem(cache.outboxKey)).toHaveLength(end);
});

it.each(['setItem','removeItem'] as const)('reports an outbox purge %s failure and can clear it later',op=>{
  const disk=outboxStorage(), key=cache.outboxKey;
  expect(cache.writeOutbox(disk,[{channel:'demo',params:{channel:'demo',body:'Unsent draft',client_key:'draft'}}])).toBe(true);
  const unavailable={...disk,[op]:()=>{throw new DOMException('Fixture storage error','SecurityError');}};
  expect(cache.purgeOutbox(unavailable,true)).toBe(false);
  expect(disk.getItem(key)).not.toBeNull();
  if(op==='removeItem') expect(cache.readOutbox(disk)).toEqual([]);
  expect(cache.purgeOutbox(disk,true)).toBe(true);
  expect(disk.getItem(key)).toBeNull();
  expect(disk.getItem(cache.outboxPurgeKey)).toBeNull();
});

it('treats corrupt, oversized or inaccessible storage as absent',()=>{
  const disk=storage();
  disk.setItem('unused','{');expect(readDisplay(disk)).toBeNull();
  disk.setItem('unused',' '.repeat(256*1024+1));expect(readDisplay(disk)).toBeNull();
  const denied={getItem(){throw new Error('denied');},setItem(){throw new Error('quota');},removeItem(){throw new Error('denied');}};
  expect(readDisplay(denied)).toBeNull();
  expect(()=>writeDisplay(denied,display() as never)).not.toThrow();
  expect(()=>clearDisplay(denied)).not.toThrow();
});

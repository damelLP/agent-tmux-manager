// Synthetic: offline demo of the browser's ATM 1.1 calls, not a seat or router simulator.
import { beforeId, isEmoji, openStatuses, postId, reactionEmoji, record } from '../src/wire.ts';
import type { ChannelRow, DecisionId, DecisionRow, Emoji, PostId, PostRow, Method, Results } from '../src/wire.ts';

const stamp = (seq:number) => new Date(Date.UTC(2026,0,1,12,0,seq)).toISOString();
const bytes = (s:string) => new TextEncoder().encode(s).length;
type Tick = {type:'event';topic:string;kind:string;seq:number;ts:string;body:object};
type Channel = Omit<ChannelRow,'seen_through'> & {
  members:{address:string;state:'starting'|'down'|null}[]; member:boolean; project:string|null; lead:string|null;
  last_post:PostId|null; last_activity:string|null; seen_through:PostId|null;
};
type Post = Omit<PostRow,'thread'|'session_id'|'last_reply_at'|'decision'|'shared'|'native'|'reactions'> & {
  also_in_chat?:true;
  thread:PostId|null; session_id:string|null; session_started:string|null; last_reply_at:string|null; task:string|null;
  decision:null; legacy:boolean; routing:'none'|'pending'; woke:string[];
};
type Choice = {by:string;decision_id:DecisionId;option:string};
type Decision = Omit<DecisionRow,'provisional'|'request'> & {
  request:DecisionRow['request'] & {recommended:string;kind:string;changes_intent:boolean;may_be_undone:boolean;files:string[];deadline:null};
  class:string; requested_at:string; taken_at:string|null; closed_at:string|null;
  withdrawal:{decision_id:DecisionId;by:string;reason:string|null}|null;
  provisional:Choice|null; resolution:(Choice & {note:null;standing_rule:boolean})|null;
};
const fail = (code:string) => ({error:{code,message:'Synthetic request refused'}});
export type FakeSeed = {pageSize?:number};
export function createFake(seed:FakeSeed = {}) {
  let seq=0;
  const ticks:Tick[] = [], listeners=new Set<(t:Tick)=>void>();
  const channels=new Map<string,Channel>(), posts=new Map<PostId,Post>(), keys=new Map<string,PostId>();
  const wakes=new Map<PostId,{to:string;message_id:PostId}[]>(), read=new Set<PostId>();
  const reacted=new Map<PostId,Map<Emoji,Set<string>>>();
  const decisions=new Map<DecisionId,Decision>();
  const agents=new Map<string,Results['agent.list'][number]>();
  let includeSeatSystem=false;
  function tick(kind:string,body:object,apply:(seq:number)=>void=()=>{}) {
    const at=++seq;
    apply(at);
    const event:Tick={type:'event',topic:kind.split('.')[0] ?? 'channel',kind,seq:at,ts:stamp(at),body};
    ticks.push(event); listeners.forEach(f=>f(event));
  }
  function addChannel(id:string,section:Channel['section'],peers:string[]) {
    const kind=section === 'channel' ? 'project' : 'direct', member=peers.includes('owner');
    channels.set(id,{id,section,kind,members:peers.map(address=>({address,state:address==='demo/chief'?'down':address.includes('/')?'starting':null})),
      member,writable:member,former:kind==='project'?['demo/chief']:[],activity_seq:0,unread:0,
      project:kind==='project'?id:null,lead:kind==='project'?`${id}/lead`:null,last_post:null,last_activity:null,seen_through:null});
  }
  const unread=(c:Channel)=>[...posts.values()].filter(p=>p.channel===c.id && (c.seen_through
    ? p.author!=='owner' && beforeId(c.seen_through,p.id)
    : p.woke.includes('owner') && !read.has(p.id))).length;
  function inject(channel:string,body:string,fields:Partial<Post>={},clientKey?:string) {
    const c=channels.get(channel);
    if (!c) return;
    const id:PostId=`m${BigInt(seq+1)}`;
    const post:Post={channel,id,body,kind:'note',author:'demo/lead',posted_at:stamp(seq+1),attachments:[],mentions:[],thread:null,
      replies:0,legacy:false,routing:'none',woke:[],session_id:null,session_started:null,task:null,last_reply_at:null,decision:null,...fields};
    const msg=(to:string)=>({from:post.author,to,body,kind:post.kind,attachments:post.attachments,task:post.task});
    tick(post.legacy?'msg.sent':'channel.posted',post.legacy ? msg(post.woke[0] ?? 'owner') :
      {channel,author:post.author,thread:post.thread,...(clientKey&&{client_key:clientKey})},at=>{
      posts.set(id,post); c.activity_seq=at; c.last_post=id; c.last_activity=post.posted_at;
      const root=post.thread ? posts.get(post.thread) : undefined;
      if (root) {root.replies++; root.last_reply_at=post.posted_at;}
    });
    wakes.set(id,post.woke.map(to=>{
      const message_id:PostId=post.legacy?id:`m${BigInt(seq+1)}`;
      if(!post.legacy)tick('msg.sent',{...msg(to),post:id});
      return {to,message_id};
    }));
    return post;
  }
  function reactAs(post:PostId,emoji:Emoji,actor:string) {
    const row=posts.get(post);
    if(!row) return fail('not_found');
    if(row.legacy||row.removed||!channels.get(row.channel)?.members.some(m=>m.address===actor)) return fail('refused');
    const byEmoji=reacted.get(post)??new Map<Emoji,Set<string>>(), actors=byEmoji.get(emoji)??new Set<string>();
    const active=!actors.has(actor);
    tick('channel.reacted',{post,actor,emoji,active},()=>{
      if(active) actors.add(actor); else actors.delete(actor);
      byEmoji.set(emoji,actors);reacted.set(post,byEmoji);
    });
    return {seq,post_id:post};
  }
  addChannel('demo','channel',['owner','demo/lead','demo/worker']);
  addChannel('demo/chief+owner','chief',['demo/chief','owner']);
  addChannel('demo/lead+demo/worker','direct',['demo/lead','demo/worker']);
  addChannel('demo/lead+owner','lead',['demo/lead','owner']);
  addChannel('owner+system','system',['owner','system']);
  inject('demo/chief+owner','The overview is ready.',{author:'demo/chief',woke:['owner']});
  const direct='demo/lead+owner';
  const dm=inject(direct,'We have <b>options</b>   for tomorrow.',{woke:['owner']});
  if(dm) {
    inject(direct,'First direct reply.',{thread:dm.id});
    inject(direct,'An ordinary direct message.');
    inject(direct,'Second direct reply.',{thread:dm.id});
  }
  inject('demo/lead+demo/worker','The read-only example.',{woke:['demo/worker']});
  inject('owner+system','A message waits for a down seat.',{author:'system',legacy:true,woke:['owner']});
  const root=inject('demo','Build notes: sample thread');
  for (let i=1;i<=62;i++) inject('demo',i===62 ? 'Literal <b>sample</b>' : `Channel post ${i}. A short example with enough words to wrap on narrow screens.`,
    i===62 ? {kind:'report',attachments:['/fixture/private/summary.txt']} : {});
  if (root) for (let i=1;i<=70;i++) inject('demo',`Thread reply ${i}. Wrapping content goes on for a while to exercise the reading position across different widths.`,{thread:root.id,author:'demo/worker'});

  function ask(title:string,taken:boolean,link?:Pick<DecisionRow,'channel'|'post_id'>) {
    const id:DecisionId=`d${BigInt(seq+1)}`, option='First sample';
    const request={requester:'demo/worker',title,context:'',options:[option,'Second sample'],recommended:option,
      kind:'plan',files:[],deadline:null,may_be_undone:true,changes_intent:true};
    const decision:Decision={id,request,status:'pending',class:'changes_intent',requested_at:stamp(seq+1),
      closed_at:null,taken_at:null,resolution:null,provisional:null,withdrawal:null,...link};
    tick('decision.requested',request,()=>decisions.set(id,decision));
    if(taken) {const choice={decision_id:id,by:'system',option};tick('decision.taken_provisionally',choice,at=>{
      decision.status='taken_provisionally';decision.provisional=choice;decision.taken_at=stamp(at);
    });}
    return id;
  }
  ask('Which sample should we use?',false);
  ask('Confirm the provisional sample?',true);
  const receipt=(post:Post)=>({seq,post_id:post.id,channel:post.channel,thread:post.thread,
    mentions:post.mentions,woke:wakes.get(post.id) ?? [],route:post.routing,unresolved:[],unavailable:[]});
  function call(method:Method,raw:unknown) {
    const p=record(raw);
    if (!p) return fail('invalid_params');
    const channel=typeof p.channel === 'string' ? p.channel : '', c=channels.get(channel);
    const lo=(id:string)=>typeof p.after !== 'string' || beforeId(p.after,id);
    const hi=(id:string)=>typeof p.before !== 'string' || beforeId(id,p.before);
    const cap=(fallback:number)=>Math.min(seed.pageSize ?? Infinity,typeof p.limit === 'number' ? p.limit : fallback);
    switch (method) {
      case 'agent.list': return [...agents.values()];
      case 'channel.list': return {seq,channels:[...channels.values()].filter(c=>includeSeatSystem||p.all===true||c.section!=='system'||c.member)
        .map(c=>({...c,unread:unread(c)}))};
      case 'channel.history': {
        if (!c) return fail('unknown_channel');
        if (p.limit!=null && (!Number.isInteger(p.limit) || Number(p.limit)<1 || Number(p.limit)>200)) return fail('invalid_params');
        const parent=p.thread==null?null:posts.get(p.thread as PostId);
        if (p.thread!=null&&!parent) return fail('not_found');
        if (parent&&parent.channel!==channel) return fail('not_in_channel');
        const root=parent?.thread ?? parent?.id ?? null;
        if(root&&p.include_shared_replies===true) return fail('invalid_params');
        const rows=[...posts.values()].filter(r=>r.channel===channel &&
          (r.thread===root || r.id===root || !root&&p.include_shared_replies===true&&r.also_in_chat) && lo(r.id) && hi(r.id));
        if (!root) rows.reverse();
        const limit=cap(50);
        return {seq,channel,posts:rows.slice(0,limit).map(r=>{
          const byEmoji=reacted.get(r.id), reactions=reactionEmoji.flatMap(emoji=>{
            const actors=byEmoji?.get(emoji);return actors?.size?[{emoji,count:actors.size,mine:actors.has('owner')}]:[];
          });
          return {...r,also_in_chat:root?undefined:r.also_in_chat,...(reactions.length&&{reactions})};
        }),more:rows.length>limit};
      }
      case 'channel.post': {
        const files=Array.isArray(p.attachments)?p.attachments:[], key=p.client_key;
        // Model channel or reply, the two targets emitted by the browser.
        if (typeof p.body!=='string' || (!p.body&&!files.length) || bytes(p.body)>16_384 || p.to!=null ||
          (p.channel!=null)===(p.reply_to!=null) || (key!=null && (typeof key!=='string' || !key || bytes(key)>64)) ||
          ('also_in_chat' in p && (p.reply_to==null || typeof p.also_in_chat!=='boolean'))) return fail('invalid_params');
        const known=typeof key==='string'?posts.get(keys.get(key) as PostId):undefined;
        if (known) return receipt(known);
        const parent=p.reply_to!=null?posts.get(p.reply_to as PostId):undefined;
        if (p.reply_to!=null&&!parent) return fail('not_found');
        const target=channels.get(parent?.channel ?? channel);
        if (!target) return fail('unknown_channel');
        if (!target.writable) return fail('refused');
        const routing=target.members.some(m=>m.address!=='owner'&&m.address!=='system')?'pending':'none';
        const posted=inject(target.id,p.body,{author:'owner',thread:parent?.thread ?? parent?.id ?? null,routing,attachments:files,
          kind:typeof p.kind==='string'?p.kind:'note',...(p.also_in_chat===true&&{also_in_chat:true})},typeof key==='string'?key:undefined);
        if (!posted) return fail('unknown_channel');
        if(typeof key==='string')keys.set(key,posted.id);
        return receipt(posted);
      }
      case 'channel.edit': case 'channel.remove': {
        const {post,body}=p, edit=method==='channel.edit';
        if(!postId(post)||Object.keys(p).length!==(edit?2:1)||edit&&(typeof body!=='string'||bytes(body)>16_384)) return fail('invalid_params');
        const row=posts.get(post);
        if(!row) return fail('not_found');
        if(row.legacy||row.removed||row.decision||row.author!=='owner') return fail('refused');
        if(typeof body==='string'&&edit) {
          if(!body.trim()&&!row.attachments.length) return fail('invalid_params');
          tick('channel.post_edited',{post,actor:'owner',body},()=>{row.body=body;row.edited=true;row.mentions=[];});
        } else tick('channel.post_removed',{post,actor:'owner'},()=>{
          Object.assign(row,{body:'removed',removed:true,attachments:[],mentions:[],task:null});delete row.edited;reacted.delete(post);
        });
        return {seq,post_id:post};
      }
      case 'channel.react': {
        const {post,emoji}=p;
        if(!postId(post)||!isEmoji(emoji)||Object.keys(p).length!==2) return fail('invalid_params');
        const alias=posts.has(post)?undefined:[...wakes].find(([,list])=>list.some(w=>w.message_id===post&&w.to==='owner'))?.at(0);
        return reactAs(typeof alias==='string'?alias:post,emoji,'owner');
      }
      case 'channel.seen': {
        const id=p.through as PostId, r=posts.get(id);
        if (!c) return fail('unknown_channel');
        if (!r) return fail('not_found');
        if (r.channel!==channel) return fail('not_in_channel');
        const changed=!c.seen_through || beforeId(c.seen_through,id);
        if (changed) tick('channel.seen',{channel,through:id,reader:'owner',session_id:null},()=>{c.seen_through=id;});
        const through=c.seen_through ?? id, message_ids:PostId[]=[];
        for(const post of posts.values()) if(post.channel===channel&&!beforeId(through,post.id)&&!read.has(post.id)) {
          const wake=wakes.get(post.id)?.find(w=>w.to==='owner');
          if(wake){message_ids.push(wake.message_id);read.add(post.id);}
        }
        if(message_ids.length)tick('msg.read',{message_ids});
        return {seq,changed,through};
      }
      case 'decision.list': {
        const statuses:readonly string[]=Array.isArray(p.statuses)?p.statuses:openStatuses;
        const all=[...decisions.values()], rows=all.filter(d=>lo(d.id)&&statuses.includes(d.status));
        const page=rows.slice(0,cap(100)), next_after=rows.length>page.length?page.at(-1)?.id ?? null:null;
        return {decisions:page,next_after,pending_count:all.filter(d=>d.status==='pending').length,
          provisional_count:all.filter(d=>d.status==='taken_provisionally').length};
      }
      case 'decision.resolve': {
        const d=decisions.get(p.id as DecisionId), option=p.option;
        if (!d) return fail('not_found');
        if (typeof option!=='string' || !d.request.options.includes(option)) return fail('refused');
        if (d.status==='resolved'||d.status==='overruled') return d.resolution?.option===option?{decision:d,changed:false}:fail('refused');
        if (!openStatuses.some(status=>status===d.status)) return fail('refused');
        const status=d.provisional&&d.provisional.option!==option?'overruled':'resolved';
        const resolution={decision_id:d.id,option,by:'owner',note:null,standing_rule:false};
        tick(`decision.${status}`,resolution,at=>{d.status=status;d.resolution=resolution;d.closed_at=stamp(at);});
        const dm='demo/worker+system';
        if(!channels.has(dm))addChannel(dm,'system',['demo/worker','system']);
        inject(dm,`Decision ${d.id} resolved: ${option}\nStanding rule: no`,{author:'system',kind:'answer',legacy:true,woke:['demo/worker']});
        return {decision:d,changed:true};
      }
      case 'decision.withdraw': {
        const d=decisions.get(p.id as DecisionId), reason=p.reason;
        if (!d) return fail('not_found');
        if (reason!=null && (typeof reason!=='string'||bytes(reason)>8192)) return fail('invalid_params');
        if (d.status==='withdrawn') return {decision:d,changed:false};
        if (d.status!=='pending') return fail('refused');
        const withdrawal={decision_id:d.id,by:'owner',reason:reason??null};
        tick('decision.withdrawn',withdrawal,at=>{d.status='withdrawn';d.withdrawal=withdrawal;d.closed_at=stamp(at);});
        return {decision:d,changed:true};
      }
    }
  }
  // Synthetic multi-project sidebar; seat+system is supplied only to this fixture.
  function sidebar() {
    includeSeatSystem=true;
    addChannel('beta','channel',['owner','beta/lead']);
    const second='demo/a+demo/b', cross='beta/lead+demo/lead', seatSystem='demo/worker+system';
    addChannel(second,'direct',['demo/a','demo/b']);
    addChannel(cross,'direct',['beta/lead','demo/lead']);
    if(!channels.has(seatSystem)) addChannel(seatSystem,'system',['demo/worker','system']);
    const first=inject(second,'Earlier read-only message',{author:'demo/a'});
    if(first) call('channel.seen',{channel:second,through:first.id});
    inject(second,'Later read-only message',{author:'demo/b'});
    inject(cross,'Cross-project fixture DM',{author:'beta/lead'});
    const target=inject(seatSystem,'Fixture seat notice',{author:'system',kind:'question'});
    return {second,cross,seatSystem,target};
  }
  return {call,inject,ask,reactAs,sidebar,hasActivity:()=>agents.size>0,
    setActivity(agent_id:string,activity:Results['agent.list'][number]['activity'],delivered=false) {
      agents.set(agent_id,{agent_id,state:'ready',activity});
      if(delivered) tick('msg.delivered',{to:agent_id});
    },watch(cursor:number,forward:(t:Tick)=>void) {
    for (const t of ticks) if (t.seq>cursor) forward(t);
    listeners.add(forward);
    return ()=>listeners.delete(forward);
  }};
}

import { canMutate, decisionBusy, decisionFailure, initial, isConversationPane, isOpen, matchesAction, maxPhotos, reduce, restoreView, roots, sameView, scopeKey, setEntry, threadHead, timeline, watchSeats } from './state.ts';
import type { Answer, ChatInput, ChatState, DecisionAction, KeepAction, OutboxItem, Phase, PullSource, ViewRef } from './state.ts';
import { beforeId, bodyError, connectFrame, daemonDown, decodeResult, openStatuses, parseFrame, pingFrame, postMethod, postMethods, refusal, requestFrame, requiredMethods, watchFrame } from './wire.ts';
import type { Activity, DecisionId, DecisionRow, Frame, Method, PostId, PostMethod, PostParams, PostRow, Requests, Results, WakeFeedback } from './wire.ts';
import type { fileHead } from './photos.ts';

export type SocketEvents = {open():void;message(raw:unknown):void;close():void};
type Socket = {send(text:string):void;close():void};
type Cancel = ()=>void;
export type SessionIO = {
  socket(events:SocketEvents):Socket; timer(fn:()=>void,ms:number):Cancel;
  online():boolean; foreground():boolean; probe(signal:AbortSignal):Promise<Phase>; uuid():string;
  upload(blob:Blob,name:string,signal:AbortSignal):Promise<{path:string;size:number}>;
  fileHead(path:string):ReturnType<typeof fileHead>;
  onNavigate?(view:ViewRef,post?:PostId):boolean;
  saveOutbox(items:readonly OutboxItem[]):boolean;
  purgeOutbox(owns:boolean):boolean; publishDenial?():void;
};
type PostChange = { [M in PostMethod]: {type:'post-change';method:M;params:Requests[M];onSent?:()=>void} }[PostMethod];
export type UiInput = PostChange | {type:'select';channel:string} | {type:'restore';view:ViewRef} | {type:'thread';id:PostId} | {type:'settings'}
  | {type:'open';id:DecisionId} | {type:'draft';key:string;text:string;start:number;end:number;share?:boolean}
  | {type:'pick';key:string;name:string;blob:Blob;photo:boolean} | {type:'remove-photo';key:string;id:string} | {type:'photo-error';key:string;message:string}
  | {type:'submit';key:string;retry?:boolean} | {type:'decision';action:DecisionAction} | {type:'keep-all';picks:readonly KeepAction[]}
  | {type:'outbox-open'|'outbox-sync';items:readonly OutboxItem[]} | {type:'outbox-close';release?:()=>void} | {type:'outbox-purge'}
  | {type:'outbox-retry'|'outbox-delete';id:string;onFailure?:()=>void}
  | {type:'visible';channel:string;through:PostId} | {type:'refresh';source:PullSource} | {type:'older'|'wake'};

export async function diagnose(fetcher:typeof fetch,signal:AbortSignal):Promise<Phase> {
  const opts:RequestInit = {mode:'same-origin',cache:'no-store',redirect:'error',signal};
  const denied = async (r:Response) => r.status === 403 ? refusal(await r.json().catch(()=>null)) : null;
  let health;
  try { health = await fetcher('/health',opts); } catch { return 'unreachable'; }
  if ([502,503,504].includes(health.status)) return 'unreachable';
  const auth = await denied(health);
  if (auth) return auth;
  if (health.ok && daemonDown(await health.json().catch(()=>null))) return 'daemon-down';
  try {
    return await denied(await fetcher('/ws',{...opts,method:'POST',referrerPolicy:'strict-origin-when-cross-origin'})) ?? 'reconnecting';
  } catch { return 'reconnecting'; }
}

type Pending = (reply?:Extract<Frame,{type:'response'}>)=>void;
type Beat = {seq:number;finish(ok:boolean):void;done:Promise<void>};
type Link = {
  ws?:Socket; requests:Map<string,Pending>; handshake?:Cancel; heartbeat?:Cancel; beat?:Beat;
  connected:boolean; watched:boolean; running:boolean; dirty:boolean; force:boolean; verifying?:boolean;
  questionRev:number; needQuestions:boolean; questions?:Promise<number|undefined>; postSeq?:number;
  shared?:boolean; heads:Set<PostId>; activity?:{wanted:boolean; reading?:boolean; sampled?:boolean};
  fileHeads:Map<string,ReturnType<typeof fileHead>>;
};
const pageSize = 50, unreadPageCap = 3, expandedHistory = {include_shared_replies:true} as const;
const unconfirmed = Symbol(), notSent = Symbol();
const postSeq=(id:PostId)=>Number(id.slice(1));
function sameDecision(a:DecisionAction,b:DecisionAction) {
  return a.option===b.option && a.pick===b.pick && a.reason===b.reason;
}

export function resetForReplay(s:ChatState,heads?:Set<PostId>) {
  heads?.clear();
  return reduce(s,{type:'reset'});
}

function wakeText({woke,unavailable,unresolved}:WakeFeedback) {
  return ([['Woken',woke.filter(to=>!unavailable.includes(to))],['Unavailable',unavailable],['Unresolved',unresolved]] as const)
    .flatMap(([label,names])=>names.length?[`${label}: ${names.join(', ')}`]:[]).join(' · ');
}

function opening(s:ChatState) {
  const row=isConversationPane(s.pane)?s.channels.find(c=>c.id===s.channel):undefined;
  return row?.unread&&row.seen_through?{after:row.seen_through}:undefined;
}

export function createSession(io:SessionIO,render:(s:ChatState)=>void,seed = initial(io.online())) {
  let s:ChatState = {...seed,opening:opening(seed)}, active:Link|undefined, cursor:number|undefined;
  let started=false, serial=0, delay=1000, decisionEpoch=0, feedbackRev=0, cachedList=seed.listSeq!==null;
  let retry:Cancel|undefined, banner:Cancel|undefined, probe:AbortController|undefined;
  let jump:{channel:string;id:PostId;serial:number}|undefined, older:string|undefined, unreadPages=0;
  const seen = new Map<string,PostId>(), seeing = new Set<string>(), hydrated = new Map<PostId,number>();
  const postReads=new Set<PostId>();
  const live = (l:Link) => active === l;
  const ready=(l:Link)=>live(l)&&s.phase==='live';
  let watched:string[]|undefined, workTick:Cancel|undefined;
  let lease:{list?:Results['channel.list'];roots:WeakSet<PostRow>;release?:()=>void}|undefined;
  let feedbackKey:string|undefined, feedbackAt=0;
  const sending=new Map<string,{cancel?:Cancel}>();
  function finishLease() {
    const release=lease?.release;
    if(release&&!sending.size) {lease=undefined;release();}
  }
  function clearSends() {for(const t of sending.values()) t.cancel?.();sending.clear();finishLease();}
  function forgetWatch() { watched=undefined; workTick?.(); workTick=undefined; }
  function update(input:ChatInput) {
    let next=input.type==='reset'?resetForReplay(s,active?.heads):reduce(s,input);
    const changedView=!sameView(s,next), departed=changedView||input.type==='reset';
    const listed=input.type==='channels'&&next!==s;
    if(changedView||(listed&&(s.listSeq===null||cachedList&&!s.opening))) {
      next.opening=opening(next); unreadPages=0;
    }
    if(listed) cachedList=false;
    if(departed) {feedbackRev++;if(next.wakeSummary) next={...next,wakeSummary:undefined};}
    if(watched&&(departed||next.phase!=='live'||
      (next.histories!==s.histories||next.channels!==s.channels)&&watchSeats(next).join()!==watched.join())) {
      forgetWatch();
      if(active?.activity) active.activity.wanted=true;
      s={...next,activity:undefined};
    } else s=next;
    render(s);
  }
  // Keep upload receipts with the pick until it is sent or removed.
  const blobs=new Map<string,{blob:Blob;path?:string;key?:string}>();
  const reset = () => { hydrated.clear(); seen.clear(); postReads.clear(); cursor=undefined; jump=undefined; decisionEpoch++; update({type:'reset'}); };
  const patch = (value:Partial<ChatState>) => update({type:'patch',value});
  const notice = (key:string,value?:string) => patch({notices:setEntry(s.notices,key,value)});
  function clearActivity() { forgetWatch(); if(s.activity) patch({activity:undefined}); }
  function purgeQueue() {
    const ok=io.purgeOutbox(!!lease);
    patch({outbox:[],outboxPurgeFailed:!ok});return ok;
  }
  function phase(value:Phase) {
    if(value==='identity'&&!purgeQueue()) io.publishDenial?.();
    if (s.phase !== value) patch({phase:value,prolonged:false,...(value==='identity'&&{pane:'home',channel:undefined,thread:undefined})});
    else if (value==='live') render(s); // recheck visibility after the completed refresh
    if (value !== 'reconnecting') { banner?.(); banner=undefined; }
    else if (!banner && !s.prolonged) banner=io.timer(()=>{banner=undefined; patch({prolonged:true});},5000);
    if (value === 'live') delay=1000;
  }
  function detach() {
    const l=active; active=undefined;
    if(lease) {lease.list=undefined;lease.roots=new WeakSet();}
    clearSends();l?.handshake?.(); l?.heartbeat?.(); l?.beat?.finish(false);
    l?.requests.forEach(done=>done()); l?.ws?.close();
    seeing.clear(); seen.clear();
    clearActivity(); clearRefresh();
    if(s.seatIds) patch({seatIds:undefined});
  }
  function clearRefresh() {if(s.refreshing) patch({refreshing:undefined});}
  function clearRetry() { retry?.(); retry=undefined; probe?.abort(); probe=undefined; }
  function lose(l:Link) {
    if (!live(l)) return;
    detach();
    if (!io.online()) { phase('offline'); return; }
    phase('reconnecting');
    const ctl = new AbortController(); probe=ctl;
    function retryAfter(value:Phase) {
      if (ctl !== probe) return;
      timeout(); probe=undefined; ctl.abort();
      if (value === 'identity') reset();
      phase(value);
      retry=io.timer(dial,delay); delay=Math.min(delay*2,30_000);
    }
    const timeout=io.timer(()=>retryAfter('reconnecting'),10_000);
    ctl.signal.addEventListener('abort',timeout,{once:true});
    io.probe(ctl.signal).then(retryAfter,()=>retryAfter('reconnecting'));
  }
  function dial() {
    clearRetry();
    if (!started || active) return;
    if (!io.online()) { phase('offline'); return; }
    if (s.phase === 'offline' || s.phase === 'reconnecting') phase('reconnecting');
    const l:Link={requests:new Map(),connected:false,watched:false,running:false,dirty:false,force:false,questionRev:0,needQuestions:true,heads:new Set(),fileHeads:new Map()};
    active=l; l.handshake=io.timer(()=>lose(l),20_000);
    try {
      l.ws=io.socket({open:()=>{if(live(l)) try {l.ws?.send(connectFrame());} catch {lose(l);}},
        message:raw=>receive(l,raw),close:()=>lose(l)});
    } catch { lose(l); }
  }
  function receive(l:Link,raw:unknown) {
    if (!live(l)) return;
    const f=parseFrame(raw);
    if (!f) return;
    if (f.type === 'connected' && !l.connected) {
      if (!requiredMethods.every(m=>f.methods.includes(m))) { lose(l); return; }
      const offered=postMethods.filter(m=>f.methods.includes(m));
      if(s.postMethods?.join()!==offered.join()) {
        for(const h of s.histories.values()) for(const id of h.rows.keys()) postReads.add(id);
        for(const id of s.heads.keys()) postReads.add(id);
        patch({postMethods:offered});
      }
      for(const [id,settled] of s.mutating??[]) if(settled) postReads.add(id);
      if(f.methods.includes('agent.list')) l.activity={wanted:true};
      l.connected=true; l.handshake?.(); phase('reconnecting'); signal(l,true); return;
    }
    if (f.type === 'response') { l.requests.get(f.id)?.(f); return; }
    if (f.type === 'pong' && l.beat?.seq === f.seq) { l.beat.finish(true); return; }
    if (f.type === 'event' && l.watched && f.seq > (cursor ?? -1)) {
      cursor=f.seq;
      const p=f.posted;
      if(p&&lease) {
        const row=s.outbox.find(o=>outboxId(o)===p.key&&o.channel===p.channel&&o.params.reply_to===p.thread&&
          o.floor!==undefined&&f.seq>o.floor&&(!o.id||o.id===p.id)&&!o.done);
        if(row) changeQueue(p.key,{id:p.id,done:true,failed:undefined});
      }
      const method=postMethod(f.kind);
      if(method) {
        if(f.post&&s.postMethods?.includes(method)) queuePost(f.post);
        return;
      }
      const decision=f.kind.startsWith('decision.');
      if (decision || f.kind === 'channel.posted') dirtyQuestions(l);
      signal(l,decision,f.kind==='msg.delivered'||f.kind==='channel.posted');
    }
    if (f.type === 'error' || f.type === 'rejected') lose(l);
  }
  function call<M extends Method>(l:Link,method:M,params:Requests[M],onSent?:()=>void):Promise<Results[M]> {
    return new Promise((resolve,reject)=>{
      if (!live(l) || !l.ws || !l.connected) { reject(notSent); return; }
      const id=`r${++serial}`;
      const required=method!=='agent.list';
      const timer=io.timer(()=>{done(); if(required) lose(l);},30_000);
      const done:Pending=reply=>{
        timer(); l.requests.delete(id);
        const data=reply && 'result' in reply && decodeResult(method,reply.result);
        if (data && live(l)) resolve(data); else reject(reply && 'error' in reply ? reply.error : unconfirmed);
      };
      l.requests.set(id,done);
      try { l.ws.send(requestFrame(id,method,params)); }
      catch { timer(); l.requests.delete(id); reject(notSent); if(required) lose(l); return; }
      onSent?.();
    });
  }
  async function readActivity(l:Link) {
    const a=l.activity;
    if(!a||!ready(l)||!io.foreground()) return;
    // An event sample waits for its history; an independent tick need not wait on other refreshes.
    if(l.running&&a.wanted) return;
    if(a.reading) {a.wanted=true;return;}
    a.wanted=false;
    const seats=watched??watchSeats(s);
    if(!seats.length&&(a.sampled||!isConversationPane(s.pane)||!s.channels.some(c=>c.id===s.channel&&c.writable))) return;
    // The array identity fences navigation and visibility round trips, even to the same seats.
    watched=seats; workTick?.(); workTick=undefined; a.reading=a.sampled=true;
    const rows=await call(l,'agent.list',{}).catch(()=>undefined);
    a.reading=false;
    if(!live(l)) return;
    const seatIds=rows?.map(row=>row.agent_id);
    if(seatIds?.join()!==s.seatIds?.join()) patch({seatIds});
    if(watched===seats&&io.foreground()) {
      const activity=new Map<string,Activity>();
      for(const id of seats) {
        const row=rows?.find(r=>r.agent_id===id);
        if(row&&row.state!=='down'&&row.state!=='stuck'&&(row.activity==='working'||row.activity==='attention_needed')) activity.set(id,row.activity);
      }
      if(activity.size||s.activity) patch({activity:activity.size?activity:undefined});
      if([...activity.values()].includes('working')) workTick=io.timer(()=>{workTick=undefined;void readActivity(l);},10_000);
    }
    if(a.wanted) void readActivity(l);
  }
  function ping(l:Link):Promise<void> {
    if (l.beat) return l.beat.done;
    const seq=++serial;
    let finish:Beat['finish']=()=>{};
    const done=new Promise<void>((resolve,reject)=>{
      const guard=io.timer(()=>finish(false),20_000);
      finish=ok=>{
        guard(); l.beat=undefined;
        if (!ok) { reject(unconfirmed); return; }
        l.heartbeat?.(); l.heartbeat=io.timer(()=>{void ping(l).catch(()=>lose(l));},60_000);
        resolve();
      };
    });
    l.beat={seq,done,finish};
    try { if (!l.ws) finish(false); else l.ws.send(pingFrame(seq)); } catch { finish(false); }
    return done;
  }
  function dirtyQuestions(l:Link) { l.questionRev++; l.needQuestions=true; }
  function questions(l:Link) {
    l.questions ??= (async ()=>{
      const at=l.questionRev, rows:DecisionRow[]=[];
      let after:DecisionId|undefined;
      do {
        const page=await call(l,'decision.list',{statuses:openStatuses,limit:100,after});
        if (!live(l) || at !== l.questionRev) return;
        rows.push(...page.decisions);
        if (page.next_after && (!page.decisions.length || (after && !beforeId(after,page.next_after)))) throw unconfirmed;
        after=page.next_after;
      } while (after);
      update({type:'decisions',rows});
      for (const [id,a] of s.answers) if (a.phase === 'unknown' && !rows.some(r=>r.id === id && isOpen(r))) answerState(id);
      return at;
    })().finally(()=>{l.questions=undefined;});
    return l.questions;
  }
  async function probeShared(l:Link,channel:string) {
    try {
      const p=await call(l,'channel.history',{channel,limit:1,...expandedHistory});
      // Only the real page is merged; it separately validates its rows and bounds.
      if (!live(l) || p.channel!==channel) throw unconfirmed;
      l.shared=true;
    } catch (error) {
      if (error!=='invalid_params' || !live(l)) throw error;
      l.shared=false;
    }
    const histories=new Map(s.histories);
    if (!l.shared && s.canShare) for (const {id} of s.channels) {
      const key=scopeKey(id), h=histories.get(key);
      if(h) histories.set(key,{...h,rows:new Map([...h.rows].filter(([,v])=>!v.row.shared))});
    }
    patch({canShare:l.shared,histories});
  }
  async function page(l:Link,params:Requests['channel.history'],extend=false) {
    const expanded=!params.thread&&l.shared, tenure=lease;
    const p=await call(l,'channel.history',{limit:pageSize,...params,...(expanded&&expandedHistory)});
    if (!live(l) || p.channel !== params.channel) throw unconfirmed;
    const posts = params.thread ? p.posts.filter(r=>r.id !== params.thread) : p.posts;
    const key=scopeKey(params.channel,params.thread);
    let last:PostId|undefined;
    for (const r of posts) {
      if (r.channel !== params.channel || (r.thread !== params.thread && !(expanded && r.shared)) ||
        (params.after && !beforeId(params.after,r.id)) || (params.before && !beforeId(r.id,params.before)) ||
        (last && !(params.thread ? beforeId(last,r.id) : beforeId(r.id,last)))) throw unconfirmed;
      last=r.id;
    }
    if (p.more && !last) throw unconfirmed;
    if(tenure&&tenure===lease) for(const r of posts) if(!r.thread) tenure.roots.add(r);
    const old=roots(s,key).at(0)?.id;
    update({type:'history',key,seq:p.seq,rows:posts,...(extend && (!old || (last ? beforeId(last,old) : params.before === old)) && {older:p.more})});
    return {more:p.more,last};
  }
  const history=(channel:string,thread?:PostId)=>s.histories.get(scopeKey(channel,thread))?.rows;
  const entries=(channel:string,thread?:PostId)=>history(channel,thread)?.values()??[];
  const replies=(channel:string,thread:PostId)=>[...entries(channel,thread)].filter(v=>v.seq>=0);
  const needsReplies=(r:PostRow)=>hydrated.get(r.id)!==r.replies && replies(r.channel,r.id).length<r.replies;
  function rememberHead(row:PostRow,seq:number) {
    if((s.heads.get(row.id)?.seq??-1)<=seq) patch({heads:setEntry(s.heads,row.id,{row,seq})});
  }
  async function readHead(l:Link,channel:string,thread:PostId) {
    const tenure=lease, p=await call(l,'channel.history',{channel,thread,limit:1}).catch(()=>undefined);
    const head=p?.posts.at(0);
    if(p?.channel!==channel||!live(l)||head?.id!==thread||head.channel!==channel||head.thread) return false;
    if(tenure===lease) tenure?.roots.add(head);
    rememberHead(head,p.seq);return true;
  }
  async function loadHeads(l:Link,channel:string) {
    for (const {row:r} of entries(channel)) {
      if (!live(l) || s.channel!==channel) return;
      const root=r.thread;
      if (!r.shared || !root || threadHead(s,channel,root) || l.heads.has(root)) continue;
      l.heads.add(root);
      // A failed lookup leaves the short label, not a broken timeline.
      await readHead(l,channel,root);
    }
  }
  async function range(l:Link,channel:string,thread?:PostId,count?:number) {
    if (thread) {
      // A cached suffix isn't a live cursor: the missing prefix must be replayed.
      let after=count===undefined&&!s.opening ? roots(s,scopeKey(channel,thread)).at(-1)?.id : replies(channel,thread).reduce((id,{row})=>beforeId(id,row.id)?row.id:id,thread);
      for (;;) {
        const next=await page(l,{channel,thread,after});
        if (!next.more) {
          // Trust the terminal page for this root and advertised count.
          if(count!==undefined) hydrated.set(thread,count);
          return;
        }
        if (!live(l) || s.channel!==channel || (count===undefined && s.thread!==thread)) return;
        after=next.last;
      }
    }
    // Scan back to the old lower boundary as well: a reply changes its root even when there is no new root.
    const known=roots(s,scopeKey(channel)), floor=known.at(0)?.id, after=known.at(-1)?.id;
    let next=await page(l,{channel,after},!floor);
    if (floor && !next.last) next=await page(l,{channel},true);
    while (floor && next.last && beforeId(floor,next.last) && live(l) && s.channel === channel) {
      next=await page(l,{channel,before:next.last},true);
      if (!next.more && !next.last) break;
    }
  }
  async function more(l:Link,channel:string) {
    const key=scopeKey(channel), before=roots(s,key).at(0)?.id;
    if (!s.histories.get(key)?.older || !before) return;
    await page(l,{channel,before},true);
  }
  function queuePost(id:PostId) {
    postReads.add(id);
    if(active?.watched&&!active.running) void refresh(active);
  }
  async function exactPost(l:Link,r:PostRow,key?:string) {
    const {id,channel,thread}=r, n=BigInt(id.slice(1));
    const params:Requests['channel.history']=key===undefined?{channel,thread:id,limit:1}:
      key===scopeKey(channel)?{channel,limit:1,...(n<18446744073709551615n&&{before:`m${n+1n}` as const}),...(l.shared&&expandedHistory)}:
      {channel,thread,after:`m${n-1n}`,limit:1};
    const p=await call(l,'channel.history',params).catch(e=>{if(!live(l)) throw e;});
    if(!live(l)) throw unconfirmed;
    const next=p?.posts.at(0);
    if(p?.channel!==channel||p.posts.length!==1||next?.id!==id||next.channel!==channel||next.thread!==thread||next.shared!==r.shared) return false;
    if(key===undefined) rememberHead(next,p.seq);
    else update({type:'history',seq:p.seq,key,rows:[next]});
    return true;
  }
  async function refreshPosts(l:Link) {
    for(const id of [...postReads]) {
      if(!live(l)) return;
      postReads.delete(id);
      const settled=s.mutating?.get(id);
      let matched=true;
      try {
        for(const [key,h] of s.histories) {
          const row=h.rows.get(id)?.row;
          if(row&&!await exactPost(l,row,key)) matched=false;
        }
        const head=s.heads.get(id)?.row;
        if(head&&!await exactPost(l,head)) matched=false;
      } catch(e) {postReads.add(id);throw e;}
      if(settled&&matched&&live(l)) patch({mutating:setEntry(s.mutating??new Map(),id,undefined),notices:setEntry(s.notices,id,undefined)});
    }
  }
  function signal(l:Link,force=false,activity=false) {
    if (!live(l) || !l.connected) return;
    if(activity&&l.activity) l.activity.wanted=true;
    l.dirty=true; l.force ||= force;
    if (!l.running) void refresh(l);
  }
  async function refresh(l:Link) {
    l.running=true;
    try {
      while (live(l) && (l.dirty||postReads.size)) {
        if(!l.dirty) {await refreshPosts(l);continue;}
        l.dirty=false;
        const force=l.force, readQuestions=l.needQuestions;
        l.force=l.needQuestions=false;
        const old=new Map(s.channels.map(c=>[c.id,c.activity_seq]));
        const tenure=lease, list=await call(l,'channel.list',{});
        if (!live(l)) return;
        let fence:Promise<void>|undefined;
        if(!l.watched&&list.seq<Math.max(cursor??0,s.listSeq??0)) {
          reset();
          // The old open thread is no longer a usable history cursor.
          if(s.thread) patch({pane:'channel',thread:undefined});
        }
        if(tenure&&tenure===lease) {
          if(!tenure.list) {
            const items=parkRolledBack(s.outbox,list.seq);
            if(items!==s.outbox) saveQueue(items,undefined,false);
          }
          tenure.list=list;
        }
        update({type:'channels',seq:list.seq,rows:list.channels});
        if (!l.watched) {
          cursor ??= list.seq;
          l.watched=true; l.ws?.send(watchFrame(cursor)); fence=ping(l);
        }
        const {channel,thread,opening:visit}=s;
        const histories = async ()=>{
          if (!channel) return;
          if (l.shared===undefined) await probeShared(l,channel);
          const key=scopeKey(channel), row=list.channels.find(c=>c.id===channel), direct=row?.kind==='direct';
          if (force || !s.histories.has(key) || old.get(channel) !== row?.activity_seq) {
            await range(l,channel);
            if (thread && (!direct || !history(channel)?.has(thread)) && s.channel===channel && s.thread===thread) await range(l,channel,thread);
          }
          if (older === channel) { older=undefined; await more(l,channel); }
          // Bound the search per visit; don't put New at the start of a partial suffix.
          if(visit&&!visit.target&&!thread) while(live(l)&&s.opening===visit&&s.histories.get(key)?.older&&
            beforeId(visit.after,roots(s,key).at(0)?.id??visit.after)) {
            if(unreadPages===unreadPageCap) { patch({opening:undefined});break; }
            unreadPages++; await more(l,channel);
          }
          const target=jump;
          while (target?.channel===channel && jump===target && live(l) && s.channel===channel) {
            const h=s.histories.get(key), low=roots(s,key).at(0)?.id;
            if (h?.rows.has(target.id)) { jump=undefined; patch({focus:{id:target.id,serial:target.serial}}); break; }
            if (!h?.older || !low || beforeId(low,target.id)) { jump=undefined; notice(key,"Couldn't find this message."); break; }
            await more(l,channel);
          }
          if (direct) for (const root of roots(s,key)) {
            if (!live(l) || s.channel!==channel) break;
            if (!root.thread && (needsReplies(root) || (root.id===s.thread && !history(channel,root.id)))) await range(l,channel,root.id,root.replies);
          }
          if (l.shared) await loadHeads(l,channel);
          if(visit&&!visit.target&&live(l)&&s.opening===visit&&row) {
            const rows=thread?[threadHead(s,channel,thread),...roots(s,scopeKey(channel,thread))]:timeline(s,row);
            const target=rows.find(r=>r&&beforeId(visit.after,r.id))?.id;
            if(target) patch({opening:{...visit,target}});
          }
        };
        await Promise.all([fence,readQuestions && questions(l),histories()]);
        if(postReads.size) await refreshPosts(l);
      }
    } catch { lose(l); }
    finally {
      l.running=false;
      if(live(l)) {
        clearRefresh();
        if(!l.verifying) {
          const rows=s.outbox.filter(o=>!o.done||o.channel===s.channel&&o.id&&
            !history(o.channel,o.params.reply_to)?.has(o.id)&&!history(o.channel)?.has(o.id));
          if(rows.length!==s.outbox.length) patch({outbox:rows});
          phase('live');drainOutbox(l);if(l.activity?.wanted) void readActivity(l);
        }
      }
    }
  }
  function select(view:ViewRef,user=false,post?:PostId) {
    if(user&&io.onNavigate?.(view,post)===false) return;
    const {pane,channel,thread}=view, refresh=isConversationPane(pane)||(pane==='home'&&channel!==s.channel);
    jump=post&&channel?{channel,id:post,serial:++serial}:undefined; older=undefined;
    patch({pane,channel,thread,focus:undefined});
    if(active&&refresh) signal(active,true,true);
  }
  function parkRolledBack(items:readonly OutboxItem[],seq:number) {
    const rolledBack=(row:OutboxItem)=>!row.done&&(row.floor??0)>seq;
    return items.some(rolledBack)?items.map(row=>rolledBack(row)?
      {...row,failed:true as const,floor:undefined,id:undefined}:row):items;
  }
  function saveQueue(items:readonly OutboxItem[],extra?:Partial<ChatState>,mustSave=true) {
    if(!lease||s.outboxPurgeFailed) return false;
    const saved=io.saveOutbox(items.filter(item=>!item.done));
    if(saved||!mustSave) patch({outbox:items,...extra});
    return saved;
  }
  const outboxId=(item:OutboxItem)=>item.params.client_key;
  const changeQueue=(id:string,value:Partial<OutboxItem>,extra?:Partial<ChatState>,mustSave=false)=>
    saveQueue(s.outbox.map(row=>outboxId(row)===id?{...row,...value}:row),extra,mustSave);
  function drainOutbox(l:Link) {
    for(const item of s.outbox) {
      // A send can synchronously detach the link and clear every ticket.
      const tenure=lease;
      if(!tenure?.list||tenure.release||s.outboxPurgeFailed||!ready(l)||!io.online()) return;
      const {channel,params}=item, id=outboxId(item), thread=params.reply_to;
      if(item.done||item.failed||sending.has(channel)) continue;
      const allowed=()=>tenure.list?.channels.some(c=>c.id===channel&&c.writable)&&(!params.also_in_chat||l.shared===true);
      if(!allowed()) {changeQueue(id,{failed:true});continue;}
      const ticket:{cancel?:Cancel}={};sending.set(channel,ticket);
      void (async()=>{
        const current=()=>live(l)&&lease===tenure&&sending.get(channel)===ticket;
        try {
          if(thread) {
            const root=threadHead(s,channel,thread);
            const proven=root&&tenure.roots.has(root)||await readHead(l,channel,thread);
            if(!current()||tenure.release||!s.outbox.some(row=>outboxId(row)===id&&!row.done)) return;
            if(!proven||!allowed()) {changeQueue(id,{failed:true});return;}
          }
          if(!changeQueue(id,{floor:item.floor??Math.max(cursor??0,tenure.list?.seq??0,l.postSeq??0)},undefined,true)) {
            changeQueue(id,{failed:true});notice(scopeKey(channel,thread),"Couldn't save the outbox.");return;
          }
          const r=await call(l,'channel.post',params);
          if(!current()) return;
          const known=s.outbox.find(row=>outboxId(row)===id)?.id;
          if(r.channel!==channel||r.thread!==params.reply_to||known&&known!==r.post_id) throw unconfirmed;
          // A receipt can beat both the watch and history for this post.
          l.postSeq=Math.max(l.postSeq??0,postSeq(r.post_id));
          const text=feedbackKey===id&&feedbackAt===feedbackRev&&r.feedback&&wakeText(r.feedback);
          changeQueue(id,{id:r.post_id,done:true},text?{wakeSummary:{key:scopeKey(channel,params.reply_to),text}}:undefined);
          signal(l,true);
        } catch(error) {
          if(!current()) return;
          const unsettled=s.outbox.some(row=>outboxId(row)===id&&!row.done);
          if(unsettled&&['invalid_params','unknown_channel','not_found','unknown_method','refused','not_in_channel'].some(code=>code===error))
            changeQueue(id,{failed:true});
          else if(unsettled&&!lease?.release) {ticket.cancel=io.timer(()=>{if(current()){sending.delete(channel);drainOutbox(l);}},4000);return;}
        } finally {
          if(current()&&!ticket.cancel) {sending.delete(channel);finishLease();drainOutbox(l);}
        }
      })();
    }
  }
  async function uploadPath(blob:Blob,name:string) {
    const ctl=new AbortController(), cancel=io.timer(()=>ctl.abort(),180_000);
    try {return (await io.upload(blob,name,ctl.signal)).path;}
    finally {cancel();}
  }
  async function submit(l:Link|undefined,key:string,retryPost:boolean) {
    const {channel,thread,phase}=s, original=s.attempts.get(key), draft=s.drafts.get(key), photos=s.photos.get(key)??[];
    const files=!!original||photos.length>0;
    if (!channel || !s.channels.some(c=>c.id===channel&&c.writable) || phase==='identity'||phase==='origin'||
      (key !== scopeKey(channel) && key !== scopeKey(channel,thread))) return;
    const share=original?original.params.also_in_chat:draft?.share;
    if ((original ? original.phase !== 'unknown' || !retryPost : retryPost) || (files&&(!l||!ready(l)||share&&!l.shared))) return;
    const body=draft?.text??'', problem=bodyError(body,files);
    if (!original && problem) { notice(key,problem === 'blank' ? 'Write a message first.' : 'Message is over 16 KiB.'); return; }
    const priorKey=photos.map(({id})=>blobs.get(id)?.key).find(Boolean);
    const fresh={body,client_key:priorKey??io.uuid(),...(thread&&key===scopeKey(channel,thread)?
      {reply_to:thread,...(share&&{also_in_chat:true as const})}:{channel})};
    const feedbackPatch={notices:setEntry(s.notices,key,undefined),wakeSummary:undefined};
    if(!files) {
      if(s.outboxPurgeFailed) purgeQueue();
      if(lease?.release||!saveQueue([...s.outbox,{channel,params:fresh}],{...feedbackPatch,drafts:setEntry(s.drafts,key,undefined)})) {
        notice(key,"Couldn't save the outbox.");return;
      }
      feedbackKey=fresh.client_key;feedbackAt=++feedbackRev;
      if(l) drainOutbox(l);
      return;
    }
    if(!l) return;
    // Uploads and their explicit Retry stay outside the text FIFO, even in the same channel.
    let params:PostParams=original?.params??fresh;
    const revision=original?.revision ?? draft?.revision ?? 0, feedback=++feedbackRev;
    patch({...feedbackPatch,attempts:setEntry(s.attempts,key,{params,revision,phase:'sending'})});
    if (!original && photos.length) {
      try {
        const attachments:string[]=[];
        for (const {id,name} of photos) {
          const pick=blobs.get(id);
          if (!pick) throw new Error("Couldn't read this file.");
          pick.path ??= await uploadPath(pick.blob,name);
          pick.key=params.client_key;
          attachments.push(pick.path);
          if (!ready(l)) throw new Error('Connection changed before sending.');
        }
        params={...params,attachments};
      } catch (error) {
        patch({attempts:setEntry(s.attempts,key,undefined),notices:setEntry(s.notices,key,
          error instanceof Error ? error.message : "Couldn't upload this file.")});
        return;
      }
    }
    try {
      const result=await call(l,'channel.post',params);
      if (!live(l) || result.channel !== channel || result.thread !== params.reply_to) throw unconfirmed;
      for (const {id} of photos) blobs.delete(id);
      const text=feedback===feedbackRev&&result.feedback&&wakeText(result.feedback);
      patch({attempts:setEntry(s.attempts,key,undefined),photos:setEntry(s.photos,key,undefined),
        ...(text&&{wakeSummary:{key,text}}),...(s.drafts.get(key)?.revision === revision && {drafts:setEntry(s.drafts,key,undefined)})});
      signal(l,true);
    } catch (error) {
      // An admission error on Retry cannot settle an earlier handoff.
      const failed=!original && (error === notSent || typeof error === 'string' &&
        ['invalid_params','unknown_channel','not_found','unknown_method'].includes(error));
      patch({attempts:setEntry(s.attempts,key,failed ? undefined : {params,revision,phase:'unknown'})});
      if (failed) notice(key,"Couldn't send this message.");
    }
  }
  async function changePost(l:Link,change:PostChange) {
    const {method,params}=change, id=params.post, c=s.channels.find(c=>c.id===s.channel);
    const row=c&&(timeline(s,c).find(r=>r.id===id)??(s.thread&&(threadHead(s,c.id,id)??history(c.id,s.thread)?.get(id)?.row)));
    if(!ready(l)||!io.online()||!row||!canMutate(s,row,method)||s.mutating?.has(id)) return;
    if(change.method==='channel.edit') {
      const problem=bodyError(change.params.body,row.attachments.length>0);
      if(problem) {notice(id,problem==='blank'?'Write a message first.':'Message is over 16 KiB.');return;}
    }
    const pending=(value?:boolean)=>patch({mutating:setEntry(s.mutating??new Map(),id,value)});
    pending(false);if(s.notices.has(id)) notice(id);
    try {
      if((await call(l,method,params,change.onSent)).post_id!==id) throw unconfirmed;
    } catch(e) {
      if(e===notSent||['invalid_params','not_found','refused','unknown_method'].some(code=>code===e)) {
        pending();notice(id,"Couldn't change this message.");return;
      }
    }
    // A receipt or an uncertain handoff both require a read, never a repeat write.
    if(s.mutating?.has(id)) {pending(true);queuePost(id);}
  }
  function answerState(id:DecisionId,value?:Answer) { patch({answers:setEntry(s.answers,id,value)}); }
  const question=(id:DecisionId)=>s.decisions.find(d=>d.id===id&&isOpen(d));
  async function mutateDecision(l:Link,action:DecisionAction,bulk=false):Promise<boolean> {
    const {id,option,pick,reason}=action, prior=s.answers.get(id), epoch=decisionEpoch;
    if(!ready(l) || !question(id)) return false;
    if(prior && (bulk || prior.phase!=='unknown' || !sameDecision(prior.action,action))) return false;
    if(!matchesAction(question(id),action)) { notice(id,decisionFailure(action)); return false; }
    answerState(id,{action,phase:'checking'});
    let uncertain=!!prior, confirmed=false;
    try {
      let rev=await questions(l);
      if(bulk && ready(l) && epoch===decisionEpoch && rev!==l.questionRev) rev=await questions(l);
      if(!ready(l) || epoch!==decisionEpoch || rev!==l.questionRev || !matchesAction(question(id),action)) return false;
      answerState(id,{action,phase:'sending'}); uncertain=true;
      const result=option===undefined?await call(l,'decision.withdraw',{id,reason}):await call(l,'decision.resolve',{id,option});
      const expected=option===undefined?'withdrawn':pick!==undefined&&pick!==option?'overruled':'resolved';
      if(!live(l) || epoch!==decisionEpoch || result.decision.id!==id || result.decision.status!==expected || result.option!==option) throw unconfirmed;
      confirmed=true;
      dirtyQuestions(l);
      update({type:'decisions',rows:s.decisions.filter(d=>d.id!==id)});
      signal(l,true);
    } catch (error) {
      if(error===notSent) uncertain=!!prior;
      if(live(l) && epoch===decisionEpoch) { dirtyQuestions(l); signal(l,true); }
    } finally {
      if(epoch===decisionEpoch) {
        const failed=!confirmed && !!question(id);
        patch({answers:setEntry(s.answers,id,failed&&uncertain?{action,phase:'unknown'}:undefined),
          notices:setEntry(s.notices,id,failed?decisionFailure(action):undefined)});
      }
    }
    return confirmed;
  }
  async function keepAll(l:Link,picks:readonly KeepAction[]) {
    const total=picks.length, epoch=decisionEpoch;
    if(total<2 || !ready(l) || decisionBusy(s)) return;
    let checked=0,kept=0;
    const progress=(running:boolean)=>patch({keepAll:{checked,kept,total,running}});
    progress(true);
    for(const pick of picks) {
      if(pick.option!==pick.pick) continue;
      if(!ready(l)) { if(question(pick.id)) notice(pick.id,decisionFailure(pick)); continue; }
      if(await mutateDecision(l,pick,true)) kept++;
      if(epoch!==decisionEpoch) return;
      checked++;progress(true);
    }
    progress(false);
  }
  async function mark(l:Link,channel:string,through:PostId) {
    if (!io.foreground() || s.phase !== 'live' || s.channel !== channel || seeing.has(channel)) return;
    const row=s.channels.find(c=>c.id===channel);
    let found=history(channel)?.has(through);
    if(row?.kind==='direct') {
      if(l.running) return;
      for(const {row:root} of entries(channel)) {
        if(root.thread) continue;
        if(needsReplies(root)) return;
        found ||= history(channel,root.id)?.has(through);
      }
    }
    found ||= (s.thread===through && !!threadHead(s,channel,through)) || history(channel,s.thread)?.has(through);
    if(!found) return;
    const confirmed=seen.get(channel), listed=row?.seen_through;
    if ((confirmed && !beforeId(confirmed,through)) || (listed && !beforeId(listed,through))) return;
    seeing.add(channel); seen.set(channel,through);
    try { const r=await call(l,'channel.seen',{channel,through}); if (live(l) && beforeId(through,r.through)) seen.set(channel,r.through); }
    catch { /* Re-list without trying the exact refused marker on each render. */ }
    finally { if (live(l)) { seeing.delete(channel); signal(l); } }
  }
  function wake() {
    if(s.outboxPurgeFailed) purgeQueue();
    if (!io.online()) { clearRetry(); detach(); phase('offline'); return; }
    if (!io.foreground()) {clearActivity();return;}
    const l=active;
    if (!l) { dial(); return; }
    if (l.watched && s.phase === 'live') {
      l.verifying=true; phase('reconnecting');
      void ping(l).then(()=>{if(live(l)) {l.verifying=false;dirtyQuestions(l);signal(l,true,true);}},()=>lose(l));
    }
  }
  return {
    start() { if (!started) { started=true; render(s); dial(); } },
    stop() { started=false; clearRetry(); banner?.(); banner=undefined; detach(); },
    photo:(id:string)=>blobs.get(id)?.blob,
    fileHead(path:string):ReturnType<typeof fileHead> {
      const l=active;
      if(!l?.connected) return Promise.resolve({});
      // Another row may render the same path before the first HEAD settles.
      const pending=l.fileHeads.get(path);
      if(pending) return pending;
      const result=io.fileHead(path).then(info=>{
        if(!info.immutable) l.fileHeads.delete(path);
        return live(l)?info:{};
      });
      l.fileHeads.set(path,result);
      return result;
    },
    async openNotice(channel:string,thread?:PostId) {
      if(!s.channels.some(c=>c.id===channel)) return false;
      const l=active;
      if(thread&&!threadHead(s,channel,thread)&&!(l&&ready(l)&&await readHead(l,channel,thread))) return false;
      if(!thread||s.thread!==thread||s.channel!==channel) {
        select({pane:'channel',channel},true);
        if(thread) select({pane:'thread',channel,thread},true);
      }
      return true;
    },
    dispatch(input:UiInput) {
      const l=active;
      switch (input.type) {
        case 'wake': wake(); break;
        case 'refresh':
          if(!l||!ready(l)||!io.online()) wake();
          else if(!s.refreshing) {patch({refreshing:input.source});l.needQuestions=true;signal(l,true);}
          break;
        case 'select': if (s.channels.some(c=>c.id===input.channel)) select({pane:'channel',channel:input.channel},true); break;
        case 'settings': if(s.pane==='home'&&s.phase!=='identity') select({pane:'settings',channel:s.channel},true); break;
        case 'restore': select(restoreView(s,input.view)); break;
        case 'thread': if (s.channel&&threadHead(s,s.channel,input.id)) select({pane:'thread',channel:s.channel,thread:input.id},true); break;
        case 'open': {
          const q=s.decisions.find(d=>d.id===input.id);
          if(q?.channel&&q.post_id&&s.channels.some(c=>c.id===q.channel)) select({pane:'channel',channel:q.channel},true,q.post_id);
          break;
        }
        case 'older': if (l && s.channel && s.phase === 'live') { older=s.channel; signal(l); } break;
        case 'draft': {
          const prior=s.drafts.get(input.key);
          patch({drafts:setEntry(s.drafts,input.key,{text:input.text,start:input.start,end:input.end,
            revision:(prior?.revision??0)+1,...((input.share??prior?.share)&&{share:true})})});break;
        }
        case 'pick': {
          const {key,name,blob,photo}=input, photos=s.photos.get(key)??[];
          if (s.attempts.has(key)||photos.length>=maxPhotos) break;
          const id=String(++serial);blobs.set(id,{blob});
          patch({photos:setEntry(s.photos,key,[...photos,{id,name,size:blob.size,photo}]),notices:setEntry(s.notices,key,undefined)});
          break;
        }
        case 'remove-photo': {
          const {key,id}=input, photos=s.photos.get(key);
          if (!s.attempts.has(key)&&photos?.some(p=>p.id===id)) {
            blobs.delete(id);const keep=photos.filter(p=>p.id!==id);
            patch({photos:setEntry(s.photos,key,keep.length?keep:undefined)});
          }
          break;
        }
        case 'photo-error': notice(input.key,input.message); break;
        case 'submit': void submit(l,input.key,input.retry===true);break;
        case 'outbox-open': {
          lease={roots:new WeakSet()};
          if(s.phase==='identity'||s.outboxPurgeFailed) purgeQueue();
          else if(input.items.some(row=>row.id)) saveQueue(input.items.map(row=>row.id?{...row,done:true,failed:undefined}:row),undefined,false);
          else patch({outbox:input.items});
          if(l) signal(l,true);break;
        }
        case 'outbox-close': {
          if(lease&&input.release) {
            // Keep the writer until actual calls settle, but don't wait on retry backoff.
            lease.release=input.release;
            for(const [channel,t] of sending) if(t.cancel) {t.cancel();sending.delete(channel);}
            finishLease();
          } else {lease=undefined;clearSends();input.release?.();}
          break;
        }
        case 'outbox-purge': purgeQueue();break;
        case 'outbox-sync':
          if(s.outboxPurgeFailed) purgeQueue();
          else if(!lease&&s.phase!=='identity') patch({outbox:input.items});
          break;
        case 'outbox-delete': case 'outbox-retry': {
          const item=s.outbox.find(row=>outboxId(row)===input.id&&row.failed);
          if(!item) break;
          const rest=s.outbox.filter(row=>row!==item);
          const items=input.type==='outbox-delete'?rest:[...rest,{...item,failed:undefined}];
          if(lease?.release||!saveQueue(items)) input.onFailure?.();
          else if(l) drainOutbox(l);
          break;
        }
        case 'post-change': if(l) void changePost(l,input); break;
        case 'visible': if (l) void mark(l,input.channel,input.through); break;
        case 'decision': if(l&&!decisionBusy(s)) void mutateDecision(l,input.action); break;
        case 'keep-all': if(l) void keepAll(l,input.picks); break;
      }
    },
  };
}

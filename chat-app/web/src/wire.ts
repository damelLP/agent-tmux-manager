// The browser/fixture seam for the ATM 1.1 envelope and §16 channel projection.
export type PostId = `m${bigint}`;
export type DecisionId = `d${bigint}`;
export type Section = 'chief' | 'lead' | 'channel' | 'direct' | 'system';
export type Mention = 'owner' | {seat:string} | {group:{project:string;name:string}};
export type ChannelRow = {
  id:string; kind:'project'|'direct'; section:Section; members:{address:string}[]; former:string[];
  writable:boolean; activity_seq:number; unread:number; seen_through?:PostId;
};
export const reactionEmoji = ['👍','✅','👀','❌'] as const;
export type Emoji = typeof reactionEmoji[number];
export const isEmoji = (v:unknown):v is Emoji => reactionEmoji.includes(v as Emoji);
type Reaction = {emoji:Emoji;count:number;mine:boolean};
export type PostRow = {
  id:PostId; channel:string; thread?:PostId; author:string; session_id?:string; kind:string; body:string;
  attachments:string[]; mentions:Mention[]; posted_at:string; replies:number; last_reply_at?:string;
  decision?:{decision_id:DecisionId;step:string}; shared?:true; native?:true; edited?:true; removed?:true; reactions?:Reaction[];
};
export const openStatuses = ['pending','taken_provisionally'] as const;
export type DecisionRow = {
  id:DecisionId; channel?:string; post_id?:PostId; status:string;
  request:{requester:string; title:string; context:string; options:string[]}; provisional?:{option:string};
};
export type Activity = 'working'|'attention_needed';
type AgentRow = {agent_id:string;state:string;activity:Activity|'idle'|null};
export type PostTarget = { channel:string; reply_to?:never; also_in_chat?:never } | { reply_to:PostId; channel?:never; also_in_chat?:true };
export type PostParams = PostTarget & {body:string;client_key:string;attachments?:string[]};
export type Requests = {
  'agent.list':Record<string,never>;
  'channel.list':Record<string,never>;
  'channel.history':{channel:string;thread?:PostId;before?:PostId;after?:PostId;limit?:number;include_shared_replies?:true};
  'channel.post':PostParams;
  'channel.edit':{post:PostId;body:string};
  'channel.remove':{post:PostId};
  'channel.react':{post:PostId;emoji:Emoji};
  'channel.seen':{channel:string;through:PostId};
  'decision.list':{statuses?:readonly (typeof openStatuses[number])[];after?:DecisionId;limit?:number};
  'decision.resolve':{id:DecisionId;option:string};
  'decision.withdraw':{id:DecisionId;reason?:string};
};
type DecisionResult = {decision:DecisionRow;changed:boolean;option?:string};
type PostReceipt = {seq:number;post_id:PostId};
export type WakeFeedback = {woke:string[];unavailable:string[];unresolved:string[]};
export type Results = {
  'agent.list':AgentRow[];
  'channel.list':{seq:number;channels:ChannelRow[]};
  'channel.history':{seq:number;channel:string;posts:PostRow[];more:boolean};
  'channel.post':{post_id:PostId;channel:string;thread?:PostId;feedback?:WakeFeedback};
  'channel.edit':PostReceipt;
  'channel.remove':PostReceipt;
  'channel.react':PostReceipt;
  'channel.seen':{changed:boolean;through:PostId};
  'decision.list':{decisions:DecisionRow[];next_after?:DecisionId};
  'decision.resolve':DecisionResult;
  'decision.withdraw':DecisionResult;
};
export type Health = {bridge:'ok';daemon:'up'|'down';protocol:string};
export const requiredMethods = ['channel.list','channel.history','channel.post','channel.seen','decision.list','decision.resolve','decision.withdraw'] as const;
const postEvents = {'channel.post_edited':'channel.edit','channel.post_removed':'channel.remove','channel.reacted':'channel.react'} as const;
export type PostMethod = typeof postEvents[keyof typeof postEvents];
export const postMethods = Object.values(postEvents);
export const methods = [...requiredMethods,...postMethods]; // advertised by the dev transport
export const postMethod = (kind:string):PostMethod|undefined => postEvents[kind as keyof typeof postEvents];
export type Method = keyof Requests;
const topics = ['channel.','decision.','msg.'];
export const version = {major:1,minor:1};
export const wsUrl = (here:Pick<Location,'protocol'|'host'>) => `${here.protocol === 'https:' ? 'wss:' : 'ws:'}//${here.host}/ws`;
const encode = (type:string, fields = {}) => JSON.stringify({type,protocol_version:version,...fields});
export const connectFrame = () => encode('connect');
export const watchFrame = (cursor:number) => encode('watch',{cursor,topics});
export const pingFrame = (seq:number) => encode('ping',{seq});
export function requestFrame<M extends Method>(id:string,method:M,params:Requests[M]) { return encode('request',{id,method,params}); }
export function beforeId(a:string,b:string) { return a.length < b.length || (a.length === b.length && a < b); }
export const bodyError = (s:string,hasPhoto=false) => !hasPhoto&&!s.trim() ? 'blank' : new TextEncoder().encode(s).length > 16_384 ? 'long' : null;
export const attachmentName=(path:string)=>path.match(/[^/\\]+$/)?.[0];
export const record = (v:unknown):Record<string,unknown>|null => v && typeof v === 'object' && !Array.isArray(v) ? v as Record<string,unknown> : null;
const text = (v:unknown):v is string => typeof v === 'string';
export const named = (v:unknown):v is string => text(v) && v.length > 0;
export const count = (v:unknown):v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
export const postId = (v:unknown):v is PostId => text(v) && /^m(?:0|[1-9]\d{0,19})$/.test(v);
const decisionId = (v:unknown):v is DecisionId => text(v) && /^d(?:0|[1-9]\d{0,19})$/.test(v);
const time = (v:unknown):v is string => text(v) && Number.isFinite(Date.parse(v));
function rows<T>(value:unknown, decode:(v:unknown)=>T|null):T[]|null {
  if (!Array.isArray(value)) return null;
  const all:T[] = [];
  for (const item of value) { const row = decode(item); if (row === null) return null; all.push(row); }
  return all;
}
const strings = (v:unknown):string[]|null => Array.isArray(v) && v.every(text) ? v : null;
type Posted = {id:PostId;key:string;channel:string;thread?:PostId};
export type Frame = {type:'connected';methods:string[]} | {type:'pong';seq:number} | {type:'event';seq:number;kind:string;post?:PostId;posted?:Posted}
  | {type:'response';id:string;result:unknown} | {type:'response';id:string;error:string} | {type:'error'|'rejected'};
export function parseFrame(raw:unknown):Frame|null {
  if (!text(raw)) return null;
  let x;
  try { x = record(JSON.parse(raw)); } catch { return null; }
  if (!x) return null;
  const {type,seq,kind,id} = x;
  if (type === 'connected') {
    const v = record(x.protocol_version), list = strings(x.methods);
    return v?.major === 1 && count(v.minor) && v.minor >= 1 && list ? {type,methods:list} : null;
  }
  if (type === 'pong' && count(seq)) return {type,seq};
  if (type === 'event' && count(seq) && text(kind) && topics.some(prefix=>kind.startsWith(prefix))) {
    const body=record(x.body), post=postMethod(kind)&&body?.post, key=body?.client_key, channel=body?.channel, thread=body?.thread;
    const posted:Posted|undefined=kind==='channel.posted'&&body?.author==='owner'&&named(key)&&named(channel)&&(thread==null||postId(thread))
      ? {id:`m${BigInt(seq)}`,key,channel,...(postId(thread)&&{thread})}:undefined;
    return {type,seq,kind,...(postId(post)&&{post}),...(posted&&{posted})};
  }
  if (type === 'error' || type === 'rejected') return {type};
  if (type === 'response' && named(id) && ('error' in x) !== ('result' in x)) {
    const error = record(x.error);
    return 'result' in x ? {type,id,result:x.result} : named(error?.code) && text(error?.message) ? {type,id,error:error.code} : null;
  }
  return null;
}
function channelRow(value:unknown):ChannelRow|null {
  const x = record(value);
  if (!x) return null;
  const {id,kind,section,writable,activity_seq,unread,seen_through} = x;
  const members = rows(x.members,v=>{ const address = record(v)?.address; return named(address) ? {address} : null; });
  const former = strings(x.former);
  if (!named(id) || (kind !== 'project' && kind !== 'direct') || !members || !former || typeof writable !== 'boolean'
    || !count(activity_seq) || !count(unread) || (section !== 'chief' && section !== 'lead' && section !== 'channel' && section !== 'direct' && section !== 'system')) return null;
  return {id,kind,section,members,former,writable,activity_seq,unread,...(postId(seen_through) && {seen_through})};
}
function mention(value:unknown):Mention|null {
  if (value === 'owner') return value;
  const x = record(value), group = record(x?.group);
  if (named(x?.seat)) return {seat:x.seat};
  return named(group?.project) && named(group?.name) ? {group:{project:group.project,name:group.name}} : null;
}
function postRow(value:unknown):PostRow|null {
  const x = record(value);
  if (!x) return null;
  const {id,channel,thread,author,session_id,kind,body,posted_at,replies,last_reply_at,also_in_chat,legacy,edited,removed} = x;
  const files = strings(x.attachments), mentions = rows(x.mentions,mention), d = record(x.decision);
  let last=-1;
  const reactions=x.reactions===undefined?[]:rows(x.reactions,v=>{
    const r=record(v), emoji=r?.emoji;
    if(!isEmoji(emoji)||!count(r?.count)||typeof r.mine!=='boolean') return null;
    const i=reactionEmoji.indexOf(emoji);
    if(i<=last) return null;last=i;
    return {emoji,count:r.count,mine:r.mine};
  });
  if (!reactions || !postId(id) || !named(channel) || !named(author) || !named(kind) || !text(body) || !time(posted_at)
    || !count(replies) || !files || !mentions || (thread != null && !postId(thread))
    || [edited,removed].some(flag=>flag!==undefined&&typeof flag!=='boolean')
    || (also_in_chat === true ? !postId(thread) || !beforeId(thread,id) : also_in_chat !== undefined && also_in_chat !== false)) return null;
  return {id,channel,author,kind,body,posted_at,replies,attachments:files,mentions,...(postId(thread) && {thread}),
    ...(legacy===false&&{native:true}),...(edited===true&&{edited:true}),...(removed===true&&{removed:true}),...(reactions.length&&{reactions}),
    ...(also_in_chat===true && {shared:true}),...(named(session_id) && {session_id}),...(time(last_reply_at) && {last_reply_at}),
    ...(decisionId(d?.decision_id) && named(d?.step) && {decision:{decision_id:d.decision_id,step:d.step}})};
}
function decisionRow(value:unknown):DecisionRow|null {
  const x = record(value), req = record(x?.request);
  if (!x || !req) return null;
  const {id,channel,post_id,status} = x, {requester,title,context} = req, options = strings(req.options), option = record(x.provisional)?.option;
  if (!decisionId(id) || !named(status) || !named(requester) || !text(title) || !text(context) || !options) return null;
  return {id,status,request:{requester,title,context,options},...(named(channel) && {channel}),
    ...(postId(post_id) && {post_id}),...(text(option) && {provisional:{option}})};
}
function agentRow(value:unknown):AgentRow|null {
  const x=record(value);
  if(!x) return null;
  const {agent_id,state,activity}=x;
  return named(agent_id) && named(state) && (activity===null || activity==='idle' || activity==='working' || activity==='attention_needed')
    ? {agent_id,state,activity} : null;
}
function receiptNames(value:unknown,key:'to'|'text'):string[] {
  return Array.isArray(value)?value.flatMap(row=>{const name=record(row)?.[key];return named(name)?[name]:[];}):[];
}
function decode(method:Method,value:unknown):Results[Method]|null {
  if(method==='agent.list') return rows(value,agentRow);
  const x = record(value);
  if (!x) return null;
  const {seq,channel,post_id,thread,more,changed,through,next_after} = x;
  switch (method) {
    case 'channel.list': { const channels = rows(x.channels,channelRow); return count(seq) && channels ? {seq,channels} : null; }
    case 'channel.history': { const posts = rows(x.posts,postRow); return count(seq) && named(channel) && typeof more === 'boolean' && posts ? {seq,channel,posts,more} : null; }
    case 'channel.post': {
      if(!postId(post_id)||!named(channel)||(thread!=null&&!postId(thread))) return null;
      const woke=receiptNames(x.woke,'to'), unavailable=receiptNames(x.unavailable,'to'), unresolved=receiptNames(x.unresolved,'text');
      return {post_id,channel,...(postId(thread)&&{thread}),...(woke.length||unavailable.length||unresolved.length?{feedback:{woke,unavailable,unresolved}}:{})};
    }
    case 'channel.edit': case 'channel.remove': case 'channel.react': return count(seq)&&postId(post_id)?{seq,post_id}:null;
    case 'channel.seen': return typeof changed === 'boolean' && postId(through) ? {changed,through} : null;
    case 'decision.list': { const decisions = rows(x.decisions,decisionRow); return decisions && (next_after == null || decisionId(next_after)) ? {decisions,...(decisionId(next_after) && {next_after})} : null; }
    case 'decision.resolve': case 'decision.withdraw': {
      const decision=decisionRow(x.decision), option=record(record(x.decision)?.resolution)?.option;
      return decision && typeof changed==='boolean' ? {decision,changed,...(text(option)&&{option})} : null;
    }
  }
}
export function decodeResult<M extends Method>(method:M,data:unknown):Results[M]|null { return decode(method,data) as Results[M]|null; }
export function refusal(value:unknown) {
  const code = record(value)?.error;
  return code === 'identity' || code === 'origin' ? code : null;
}
export function daemonDown(value:unknown) {
  const x = record(value);
  return x?.bridge === 'ok' && x.daemon === 'down' && named(x.protocol);
}

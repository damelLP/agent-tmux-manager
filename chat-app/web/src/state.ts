import { beforeId } from './wire.ts';
import type { Activity, ChannelRow, DecisionId, DecisionRow, PostId, PostMethod, PostParams, PostRow } from './wire.ts';

export type Phase = 'live'|'reconnecting'|'offline'|'unreachable'|'daemon-down'|'identity'|'origin';
export type Pane = 'home'|'channel'|'thread'|'settings';
export const isConversationPane = (pane:Pane) => pane==='channel'||pane==='thread';
export type PullSource = 'side'|'channel';
export type Draft = {text:string;revision:number;start:number;end:number;share?:true};
export type Photo = {id:string;name:string;size:number;photo:boolean};
export const maxPhotos=8;
export type Attempt = {params:PostParams;revision:number;phase:'sending'|'unknown'};
export type OutboxItem = {channel:string;params:PostParams & {attachments?:never};floor?:number;id?:PostId;failed?:true;done?:true};
type ResolveAction = {id:DecisionId;option:string;pick?:string;reason?:never};
export type KeepAction = ResolveAction & {pick:string};
export type DecisionAction = ResolveAction | {id:DecisionId;option?:never;pick?:never;reason?:string};
export type Answer = {action:DecisionAction;phase:'checking'|'sending'|'unknown'};
type VersionedPost = {row:PostRow;seq:number};
export type History = {rows:ReadonlyMap<PostId,VersionedPost>;older:boolean};
export type ChatState = {
  phase:Phase; prolonged:boolean; refreshing?:PullSource; listSeq:number|null; channels:ChannelRow[]; decisions:DecisionRow[];
  histories:ReadonlyMap<string,History>; heads:ReadonlyMap<PostId,VersionedPost>; canShare?:boolean;
  postMethods?:readonly PostMethod[]; mutating?:ReadonlyMap<PostId,boolean>;
  pane:Pane; channel?:string; thread?:PostId; focus?:{id:PostId;serial:number};
  opening?:{after:PostId;target?:PostId};
  activity?:ReadonlyMap<string,Activity>; seatIds?:readonly string[];
  wakeSummary?:{key:string;text:string};
  drafts:ReadonlyMap<string,Draft>; attempts:ReadonlyMap<string,Attempt>; answers:ReadonlyMap<DecisionId,Answer>;
  photos:ReadonlyMap<string,readonly Photo[]>; outbox:readonly OutboxItem[]; outboxPurgeFailed?:boolean;
  notices:ReadonlyMap<string,string>; keepAll?:{total:number;checked:number;kept:number;running:boolean};
};
export type ViewRef = Pick<ChatState,'pane'|'channel'|'thread'>;
export type ChatInput = {type:'patch';value:Partial<ChatState>} | {type:'channels';seq:number;rows:ChannelRow[]}
  | {type:'decisions';rows:DecisionRow[]} | {type:'history';seq:number;key:string;rows:PostRow[];older?:boolean} | {type:'reset'};
export const scopeKey = (channel:string,thread?:PostId) => JSON.stringify([channel,thread ?? null]);
export const initial = (online = true):ChatState => ({phase:online ? 'reconnecting' : 'offline',prolonged:false,listSeq:null,
  channels:[],decisions:[],histories:new Map(),heads:new Map(),pane:'home',drafts:new Map(),attempts:new Map(),answers:new Map(),notices:new Map(),photos:new Map(),outbox:[]});
export const sameView = (a:ViewRef,b:ViewRef) => a.pane===b.pane&&a.channel===b.channel&&a.thread===b.thread;
export function restoreView(s:ChatState,v:ViewRef):ViewRef {
  const channel=s.channels.some(c=>c.id===v.channel)?v.channel:s.channel;
  const thread=v.pane==='thread'&&channel&&channel===v.channel&&threadHead(s,channel,v.thread)?v.thread:undefined;
  return {channel,thread,pane:v.pane==='thread'&&!thread?'channel':v.pane};
}
export function provisionalPick(row:DecisionRow):string|undefined {
  const pick=row.provisional?.option;
  return row.status==='taken_provisionally' && pick!==undefined && row.request.options.includes(pick) ? pick : undefined;
}
export function allowedOptions(row:DecisionRow):readonly string[] {
  return row.status==='pending' || provisionalPick(row)!==undefined ? row.request.options : [];
}
export function matchesAction(row:DecisionRow|undefined,a:DecisionAction) {
  return row?.id===a.id && (a.pick===undefined?row.status==='pending':provisionalPick(row)===a.pick) &&
    (a.option===undefined || row.request.options.includes(a.option));
}
export function decisionBusy(s:ChatState) { return !!s.keepAll?.running || [...s.answers.values()].some(a=>a.phase!=='unknown'); }
export function eligiblePicks(s:ChatState):KeepAction[] {
  return s.decisions.flatMap(row=>{
    const option=provisionalPick(row);
    return option===undefined||s.answers.has(row.id)?[]:[{id:row.id,option,pick:option}];
  });
}
export function decisionFailure(a:DecisionAction) {
  return a.option===undefined?"Couldn't confirm that withdrawal.":"Couldn't confirm that answer.";
}
export function isOpen(row:DecisionRow) { return row.status === 'pending' || row.status === 'taken_provisionally'; }
export function cards(s:ChatState) {
  return s.decisions.filter(isOpen);
}
const byId=(a:PostRow,b:PostRow)=>a.id===b.id?0:beforeId(a.id,b.id)?-1:1;
export function roots(s:ChatState,key:string):PostRow[] {
  return [...(s.histories.get(key)?.rows.values() ?? [])].map(v=>v.row).sort(byId);
}
export function threadHead(s:ChatState,channel:string,id?:PostId) {
  if(!id) return;
  const main=s.histories.get(scopeKey(channel))?.rows.get(id), head=s.heads.get(id);
  const row=(main&&(!head||main.seq>=head.seq)?main:head)?.row;
  return row?.channel===channel&&!row.thread?row:undefined;
}
export function canMutate(s:ChatState,row:PostRow,method:PostMethod) {
  return s.phase==='live'&&s.postMethods?.includes(method)&&row.native&&!row.removed&&
    (method==='channel.react'||row.author==='owner'&&!row.decision)&&s.channels.some(c=>c.id===row.channel&&c.writable);
}
export const incomingOwnerMention = ({author,mentions}:Pick<PostRow,'author'|'mentions'>) => author!=='owner'&&mentions.includes('owner');
export function timeline(s:ChatState,channel:ChannelRow):PostRow[] {
  const key=scopeKey(channel.id), rows=roots(s,key), main=s.histories.get(key)?.rows;
  return channel.kind==='direct' ? rows.flatMap(r=>r.thread?[r]:[r,...roots(s,scopeKey(channel.id,r.id)).filter(p=>!main?.has(p.id))]).sort(byId) : rows;
}
export function watchSeats(s:ChatState):string[] {
  const channel=s.channels.find(c=>c.id===s.channel);
  if(!isConversationPane(s.pane)||!channel) return [];
  if(channel.kind==='direct') {
    const peer=channel.members.find(m=>m.address!=='owner')?.address;
    return peer?.includes('/')&&channel.members.length===2&&channel.members.some(m=>m.address==='owner')&&
      !channel.former.includes(peer)?[peer]:[];
  }
  const thread=s.pane==='thread'?s.thread:undefined;
  let last:PostRow|undefined;
  for(const {row} of s.histories.get(scopeKey(channel.id,thread))?.rows.values()??[]) {
    if(row.author==='owner'&&(row.thread===thread||!thread&&row.shared)&&(!last||beforeId(last.id,row.id))) last=row;
  }
  return [...new Set(last?.mentions.flatMap(m=>m!=='owner'&&'seat' in m?[m.seat]:[]))];
}
export function setEntry<K,V>(map:ReadonlyMap<K,V>,key:K,value:V|undefined) {
  const next = new Map(map);
  if (value === undefined) next.delete(key); else next.set(key,value);
  return next;
}
export function reduce(s:ChatState,input:ChatInput):ChatState {
  switch (input.type) {
    case 'patch': return {...s,...input.value};
    case 'reset': return {...s,channels:[],listSeq:null,decisions:[],histories:new Map(),heads:new Map(),focus:undefined,opening:undefined,activity:undefined,seatIds:undefined,
      keepAll:undefined,mutating:undefined,answers:new Map(),notices:new Map([...s.notices].filter(([key])=>key.startsWith('[')))};
    case 'channels': {
      if (s.listSeq !== null && input.seq < s.listSeq) return s;
      const channel = input.rows.some(c=>c.id === s.channel) ? s.channel : (input.rows.find(c=>c.kind === 'project') ?? input.rows.at(0))?.id;
      return {...s,channels:input.rows,listSeq:input.seq,channel,
        ...(channel !== s.channel && {thread:undefined,focus:undefined,pane:s.pane==='thread'?'channel':s.pane})};
    }
    case 'decisions': return {...s,decisions:input.rows};
    case 'history': {
      const prior = s.histories.get(input.key), rows = new Map(prior?.rows);
      for (const row of input.rows) if ((rows.get(row.id)?.seq ?? -1) <= input.seq) rows.set(row.id,{row,seq:input.seq});
      return {...s,histories:setEntry(s.histories,input.key,{rows,older:input.older ?? prior?.older ?? false})};
    }
  }
}

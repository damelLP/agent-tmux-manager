import { reduce, roots, scopeKey, timeline } from './state.ts';
import type { ChatState, Draft, OutboxItem } from './state.ts';
import { attachmentName, bodyError, count, decodeResult, named, postId, record } from './wire.ts';
import type { ChannelRow, DecisionRow, PostId, PostRow } from './wire.ts';

const displayKey='atm-chat-display-1', resumeKey='atm-chat-resume-1', budget=256*1024;
const bytes=(value:string)=>new TextEncoder().encode(value).length;
type Disk = Pick<Storage,'getItem'|'setItem'|'removeItem'>|undefined;
export const outboxKey='atm-chat-outbox-1', outboxPurgeKey=outboxKey+'-denied';
const outboxBytes=128*1024, outboxCount=64;
export function outboxDenied(disk:Disk) {try {return disk?.getItem(outboxPurgeKey)==='1';} catch {return undefined;}}
export function purgeOutbox(disk:Disk,owns:boolean) {
  if(!disk) return false;
  // Any tab can persist denial; only the writer can erase the queue and clear the fence.
  try {
    disk.setItem(outboxPurgeKey,'1');
    if(owns) {disk.removeItem(outboxKey);disk.removeItem(outboxPurgeKey);}
    return true;
  } catch {return false;}
}
export function readOutbox(disk:Disk):OutboxItem[] {
  if(outboxDenied(disk)!==false) return [];
  try {
    const data=disk?.getItem(outboxKey);
    const raw=data&&bytes(data)<=outboxBytes?JSON.parse(data):null;
    if(!Array.isArray(raw)||raw.length>outboxCount) return [];
    const keys=new Set<string>();
    return raw.flatMap((row:unknown):OutboxItem[]=>{
      if(!Array.isArray(row)||row.length!==8) return [];
      const [channel,key,body,thread,share,floor,id,failed]=row;
      if(!named(channel)||!named(key)||bytes(key)>64||keys.has(key)||typeof body!=='string'||bodyError(body)||
        thread!==null&&!postId(thread)||share!==null&&(share!==true||!thread)||failed!==null&&failed!==true||
        floor!==null&&!count(floor)||id!==null&&(!postId(id)||floor===null)) return [];
      keys.add(key);
      return [{channel,params:{body,client_key:key,...(thread?{reply_to:thread,also_in_chat:share===true?true:undefined}:{channel})},
        floor:floor??undefined,id:id??undefined,...(id?{done:true}:failed===true?{failed:true}:{})}];
    });
  } catch {return [];}
}
export function writeOutbox(disk:Disk,items:readonly OutboxItem[]) {
  if(!disk||items.length>outboxCount) return false;
  const data=JSON.stringify(items.map(({channel,params:p,floor,id,failed})=>[channel,p.client_key,p.body,p.reply_to,p.also_in_chat,floor,id,failed]));
  // Leave room for a 16-digit floor and a quoted 21-character post ID.
  const reserve=items.reduce((n,{floor,id})=>n+37-String(floor??null).length-(id?.length??2),0);
  if(bytes(data)+reserve>outboxBytes) return false;
  try {
    const flag=disk.getItem(outboxPurgeKey);
    if(flag==='1') return false;
    if(items.length) {
      // Reserve the denial flag before accepting text, even if the store then fills up.
      if(!flag) disk.setItem(outboxPurgeKey,'0');
      disk.setItem(outboxKey,data);
    } else disk.removeItem(outboxKey);
    return true;
  } catch {return false;}
}
export type DisplayProjection = {seq:number;channels:ChannelRow[];decisions:DecisionRow[];roots:PostRow[];replies:PostRow[];
  channel?:string;thread?:PostId;older:boolean};
function project(value:unknown):DisplayProjection|null {
  const x=record(value), list=decodeResult('channel.list',x);
  const questions=decodeResult('decision.list',x);
  if (!x || !list || !questions) return null;
  const selected=list.channels.find(c=>c.id===x.channel), channel=selected?.id;
  const page=(posts:unknown)=>decodeResult('channel.history',{seq:list.seq,channel:channel??'-',posts,more:false})?.posts
    .map(row=>({...row,attachments:row.attachments.flatMap(path=>attachmentName(path)??[])}));
  const main=page(x.roots), nested=page(x.replies);
  if (!main || !nested) return null;
  const roots=main.filter(r=>r.channel===channel&&!r.thread), thread=roots.find(r=>r.id===x.thread)?.id;
  return {...list,...questions,roots,replies:nested.filter(r=>r.channel===channel&&r.thread&&
    (thread ? r.thread===thread : selected?.kind==='direct'&&roots.some(p=>p.id===r.thread))),
    channel,thread,older:x.older===true};
}
export function readDisplay(disk:Disk) {
  try { const data=disk?.getItem(displayKey); return data&&bytes(data)<=budget?project(JSON.parse(data)):null; }
  catch { return null; }
}
export function writeDisplay(disk:Disk,value:DisplayProjection) {
  if(!disk) return;
  const view=project(value);
  if(!view) return;
  if(view.roots.length>30){view.roots=view.roots.filter((r,i,all)=>r.id===view.thread || i>=all.length-(view.thread ? 29 : 30));view.older=true;}
  view.replies=view.replies.filter(r=>view.roots.some(p=>p.id===r.thread)).slice(-30);
  let out=JSON.stringify(view);
  while (bytes(out)>budget) {
    if(view.replies.length) view.replies.shift();
    else {
      const i=view.roots.findIndex(r=>r.id!==view.thread);
      if(i>=0) {view.roots.splice(i,1);view.older=true;}
      else if(view.decisions.length) view.decisions.pop();
      else return;
    }
    out=JSON.stringify(view);
  }
  try {disk.setItem(displayKey,out);} catch { /* Private browsing or quota. */ }
}
export function clearDisplay(disk:Disk) {try {disk?.removeItem(displayKey);} catch { /* No storage access. */ }}
export function snapshot(s:ChatState):DisplayProjection {
  const {channels,decisions,channel,thread}=s, key=scopeKey(channel??''), selected=channels.find(c=>c.id===channel);
  return {seq:s.listSeq??0,channels,decisions,channel,thread,roots:roots(s,key),
    replies:thread?roots(s,scopeKey(channel??'',thread)):selected?.kind==='direct'?timeline(s,selected).filter(r=>r.thread):[],older:s.histories.get(key)?.older??false};
}
export function seedDisplay(seed:ChatState,view:DisplayProjection|null) {
  if(!view) return seed;
  const {seq,channels,decisions,channel,thread}=view;
  let s=reduce({...seed,channel},{type:'channels',seq,rows:channels});
  if(channel){s=reduce(s,{type:'history',key:scopeKey(channel),seq:-1,rows:view.roots,older:view.older});
    if(thread) s=reduce(s,{type:'history',key:scopeKey(channel,thread),seq:-1,rows:view.replies});
    else for(const r of view.replies) if(r.thread) s=reduce(s,{type:'history',key:scopeKey(channel,r.thread),seq:-1,rows:[r]});}
  return {...s,decisions};
}
export function browserStorage(which:'localStorage'|'sessionStorage'):Disk {try{return window[which];}catch{return undefined;}}
export function saveResume(disk:Disk,s:ChatState,marks:unknown) {
  const drafts=[...s.drafts].filter(([,draft])=>draft.text||draft.share);
  try {if(!disk)return !drafts.length;disk.setItem(resumeKey,JSON.stringify({drafts,marks}));return true;}
  catch {return !drafts.length;}
}
export function takeResume(disk:Disk,s:ChatState) {
  try {
    const value=disk?.getItem(resumeKey);
    if(!value) return {s};
    disk?.removeItem(resumeKey);
    const x=record(JSON.parse(value)), drafts=new Map<string,Draft>();
    if(!x || !Array.isArray(x.drafts)) return {s};
    for(const item of x.drafts) {
      const d=Array.isArray(item)?record(item.at(1)):null, key=Array.isArray(item)?item.at(0):null;
      if(typeof key==='string'&&typeof d?.text==='string'&&typeof d.start==='number'&&typeof d.end==='number')
        drafts.set(key,{text:d.text,start:d.start,end:d.end,revision:0,...(d.share===true&&{share:true})});
    }
    return {s:{...s,drafts},marks:x.marks};
  } catch {return {s};}
}

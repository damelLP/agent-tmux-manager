import { sameView } from './state.ts';
import type { ViewRef } from './state.ts';
import { count, named, postId, record } from './wire.ts';
import type { PostId } from './wire.ts';

type Entry = {atmNav:1;run:string;id:number;depth:number;view:ViewRef;post?:PostId};
type Port = Pick<History,'state'|'length'|'pushState'|'replaceState'|'go'|'back'>;
export function readEntry(value:unknown):Entry|undefined {
  const e=record(value), v=record(e?.view);
  if(e?.atmNav!==1||!named(e.run)||!count(e.id)||!count(e.depth)||e.depth>e.id||!v) return;
  const {pane,channel,thread}=v;
  if((pane!=='home'&&pane!=='channel'&&pane!=='thread'&&pane!=='settings')||(pane==='home')!==(e.depth===0)||
    (channel!==undefined&&!named(channel))||(pane==='thread'? !channel||!postId(thread) : thread!==undefined)||
    (e.post!==undefined&&(pane!=='channel'||!postId(e.post)))) return;
  return e as Entry;
}

export function createNavigation(h:Port,run:string,home:ViewRef,resume:boolean,restore:(view:ViewRef)=>void, index:()=>number|undefined) {
  const prior=readEntry(h.state);
  let current:Entry|undefined, serial=0, rewind=false, stale:string|undefined;
  function write(depth:number,id:number,view:ViewRef,push=false,post?:PostId) {
    const entry:Entry={atmNav:1,run,depth,id,view,post};
    try { if(push) h.pushState(entry,'');else h.replaceState(entry,''); }
    catch { return false; }
    current=entry;return true;
  }
  function anchor(from?:Entry) {
    current=undefined;
    if(from) {
      const {depth}=from, steps=Math.min(depth,index()??(depth<h.length?depth:0));
      if(steps>0) try {
        h.go(-steps);rewind=true;stale=from.run;return;
      } catch { /* History unavailable. */ }
    }
    write(0,0,home);
  }
  if(resume&&prior) {current=prior;run=prior.run;serial=prior.id;}
  else anchor(prior);
  return {
    push(view:ViewRef,post?:PostId) {
      if(!current) return false;
      if(sameView(view,current.view)&&(!post||post===current.post)) return true;
      return write(current.depth+1,++serial,view,true,post);
    },
    pop(value:unknown) {
      // A rewind lands at Home even if the slot was replaced in the meantime.
      const e=rewind?undefined:readEntry(value);
      rewind=false;
      if(e?.run===run) {
        current=e;serial=Math.max(serial,e.id);restore(e.view);return;
      }
      anchor(e?.run===stale?e:undefined);
      restore(home);
    },
    rewinding:()=>rewind,
    align({pane,channel,thread}:ViewRef) {
      const view={pane,channel,thread};
      if(current&&!sameView(view,current.view)&&write(current.depth,current.id,view)&&pane==='home') home=view;
    },
    reset(nextRun:string) {run=nextRun;serial=0;home={pane:'home'};anchor(current);},
    back() {if(current?.depth) h.back();},
    entryKey:()=>current?.id,
    view:()=>current?.view??home,
  };
}

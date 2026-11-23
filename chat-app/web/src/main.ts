import { browserStorage, clearDisplay, outboxDenied, outboxKey, outboxPurgeKey, purgeOutbox, readDisplay, readOutbox, saveResume, seedDisplay, snapshot, takeResume, writeDisplay, writeOutbox } from './cache.ts';
import { createNavigation } from './navigation.ts';
import { registerPwa } from './pwa.ts';
import { fileHead, upload } from './photos.ts';
import { createSession, diagnose } from './session.ts';
import type { UiInput } from './session.ts';
import { initial, restoreView } from './state.ts';
import type { ViewRef } from './state.ts';
import { mount } from './view.ts';
import { postId, wsUrl } from './wire.ts';

const app=document.getElementById('app');
if (app) {
  const disk=browserStorage('localStorage'), scratch=browserStorage('sessionStorage');
  let denials:BroadcastChannel|undefined;
  try {denials=new BroadcastChannel(outboxPurgeKey);} catch { /* Storage events still cover a durable denial. */ }
  const boot=takeResume(scratch,seedDisplay({...initial(navigator.onLine),outbox:readOutbox(disk)},readDisplay(disk)));
  const home:ViewRef={pane:'home',channel:boot.s.channel};
  // The browser can prune early slots. The index bounds a rewind even with Forward entries.
  const index=()=>(window as Window & {navigation?:{currentEntry?:NavigationHistoryEntry}}).navigation?.currentEntry?.index;
  const navigation=createNavigation(history,crypto.randomUUID(),home,Array.isArray(boot.marks),view=>session.dispatch({type:'restore',view}),index);
  let state={...boot.s,...restoreView(boot.s,navigation.view())}, persisted=boot.s;
  history.scrollRestoration='manual';
  let pending:Extract<UiInput,{type:'select'|'open'|'thread'|'settings'}>|undefined;
  let notice=location.hash.slice(1)||undefined;
  if(notice) history.replaceState(history.state,'',location.pathname+location.search);
  function noticeHome(decision?:string) {
    if(state.pane!=='home') {
      navigation.reset(crypto.randomUUID());
      session.dispatch({type:'restore',view:{pane:'home'}});
    }
    if(decision) document.getElementById(decision)?.focus();
  }
  function openNotice() {
    if(notice===undefined||navigation.rewinding()||notice&&state.phase==='reconnecting') return;
    const route=new URLSearchParams(notice);notice=undefined;
    const channel=route.get('c'), thread=route.get('t'), decision=route.get('d');
    if(channel&&(thread===null||postId(thread))) {
      void session.openNotice(channel,thread??undefined).then(open=>{if(!open) noticeHome();});
    } else noticeHome(state.decisions.find(q=>q.id===decision)?.id);
  }
  const view=mount(app,input=>{
    if(input.type==='back') navigation.back();
    else if(navigation.rewinding()&&(input.type==='select'||input.type==='open'||input.type==='thread'||input.type==='settings')) pending=input;
    else session.dispatch(input);
  },boot.marks,id=>session.photo(id),path=>session.fileHead(path));
  const session=createSession({
    upload,fileHead,saveOutbox:items=>writeOutbox(disk,items),purgeOutbox:owns=>purgeOutbox(disk,owns),
    publishDenial:()=>denials?.postMessage('purge'),
    socket(events) {
      const ws=new WebSocket(wsUrl(location));
      ws.addEventListener('open',events.open);
      ws.addEventListener('message',e=>events.message(e.data));
      ws.addEventListener('close',events.close);
      return {send:text=>{if(ws.readyState !== WebSocket.OPEN) throw new Error('Socket closed'); ws.send(text);},close:()=>ws.close()};
    },
    timer(fn,ms) {const id=window.setTimeout(fn,ms);return ()=>window.clearTimeout(id);},
    online:()=>navigator.onLine,foreground:()=>document.visibilityState==='visible',
    probe:signal=>diagnose(fetch,signal),uuid:()=>crypto.randomUUID(),onNavigate:navigation.push,
  },next=>{
    if(next.phase==='identity'&&state.phase!=='identity') {pending=undefined;navigation.reset(crypto.randomUUID());}
    navigation.align(next);
    state=next;view.render(next,navigation.entryKey());
    if(next.phase==='identity') {clearDisplay(disk);persisted=next;}
    if(next.phase==='live' && (next.histories!==persisted.histories || next.channels!==persisted.channels ||
      next.decisions!==persisted.decisions || next.thread!==persisted.thread || next.channel!==persisted.channel)) {
      writeDisplay(disk,snapshot(next));persisted=next;
    }
    openNotice();
  },state);
  navigator.serviceWorker?.addEventListener('message',e=>{
    if(typeof e.data?.notice==='string') {notice=e.data.notice;openNotice();}
  });
  registerPwa(()=>!state.attempts.size&&!state.answers.size&&!state.keepAll?.running,()=>saveResume(scratch,state,view.bookmarks()));
  const wake=()=>session.dispatch({type:'wake'});
  let lease:AbortController|undefined, holding=false;
  function releaseOutbox(force=false) {
    const owner=lease;
    if(!owner) return;
    const release=()=>{
      lease=undefined;holding=false;owner.abort();
      if(document.visibilityState==='visible'&&!force) claimOutbox();
    };
    if(holding&&!force) session.dispatch({type:'outbox-close',release});
    // A forced unload cannot wait for the old RPC; a new owner relies on same-key dedupe.
    else {session.dispatch({type:'outbox-close'});release();}
  }
  function claimOutbox() {
    if(document.visibilityState!=='visible') {releaseOutbox();return;}
    if(lease||!navigator.locks) return;
    const owner=new AbortController();lease=owner;
    void navigator.locks.request(outboxKey,{signal:owner.signal},async()=>{
      if(owner.signal.aborted) return;
      holding=true;
      const release=new Promise<void>(done=>owner.signal.addEventListener('abort',()=>done(),{once:true}));
      session.dispatch({type:'outbox-open',items:readOutbox(disk)});
      if(outboxDenied(disk)) session.dispatch({type:'outbox-purge'});
      await release;
    }).catch(()=>{if(lease===owner) releaseOutbox(true);});
  }
  denials?.addEventListener('message',e=>{if(e.data==='purge') session.dispatch({type:'outbox-purge'});});
  addEventListener('online',wake); addEventListener('offline',wake);
  document.addEventListener('visibilitychange',()=>{claimOutbox();wake();});
  addEventListener('storage',e=>{
    if(e.key===outboxPurgeKey&&e.newValue==='1'&&outboxDenied(disk)) session.dispatch({type:'outbox-purge'});
    else if(e.key===outboxKey||e.key===null) session.dispatch({type:'outbox-sync',items:readOutbox(disk)});
  });
  const pop=(e:PopStateEvent)=>{
    navigation.pop(e.state);
    if(pending&&!navigation.rewinding()) {
      const input=pending;pending=undefined;session.dispatch(input);
    }
  };
  addEventListener('popstate',pop);
  addEventListener('pagehide',e=>{releaseOutbox(true);if(!e.persisted) {removeEventListener('popstate',pop);session.stop();view.dispose();}});
  addEventListener('pageshow',e=>{if(e.persisted) {claimOutbox();wake();}});
  session.start();claimOutbox();
}

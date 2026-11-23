import { expect, it } from 'vitest';
import { createNavigation, readEntry } from '../src/navigation.ts';
import type { ViewRef } from '../src/state.ts';

const home={pane:'home'} as const, channel={pane:'channel',channel:'demo'} as const;
const thread={...channel,pane:'thread',thread:'m9'} as const;
const settings={...channel,pane:'settings'} as const;
const entry=(view:ViewRef,depth=0,id=depth,run='old')=>({atmNav:1,run,id,depth,view});

function fixture(state:unknown=null,available?:number) {
  const restored:ViewRef[]=[], pushed:unknown[]=[], steps:number[]=[];
  const port={length:4,state,pushState(v:unknown){this.state=v;pushed.push(v);},replaceState(v:unknown){this.state=v;},
    go(n:number){steps.push(n);},back(){this.go(-1);}};
  const nav=(resume=false)=>createNavigation(port,'new',home,resume,v=>restored.push(v),()=>available);
  const pop=(n:ReturnType<typeof nav>,v:unknown)=>{port.state=v;n.pop(v);};
  return {port,nav,pop,pushed,steps,restored};
}

it('rejects states that cannot be restored as app history',()=>{
  for(const v of [null,{...entry(home),atmNav:2},entry(home,0.5),entry(home,-1),entry(home,2,1),
    {...entry(thread,2),view:{pane:'thread',channel:'demo'}},
    {...entry(thread,2),view:{...thread,thread:'m009'}},entry(thread,0),
    {...entry(channel,1),post:'m009'}]) expect(readEntry(v)).toBeUndefined();
  expect(readEntry(entry(thread,2))).toMatchObject(entry(thread,2));
});

it('accepts Settings as a non-root entry with no thread or linked post',()=>{
  expect(readEntry(entry(settings,1))).toEqual(entry(settings,1));
  for(const v of [entry(settings),{...entry(settings,1),view:{...settings,thread:'m9'}},
    {...entry(settings,1),post:'m9'}]) expect(readEntry(v)).toBeUndefined();
});

it('rewinds Settings on ordinary reload and resumes it after a worker update',()=>{
  const loading=fixture(entry(settings,1));
  const reset=loading.nav();
  expect(loading.steps).toEqual([-1]);
  loading.pop(reset,entry(home));
  expect(reset.view()).toEqual(home);
  const updating=fixture(entry(settings,1));
  const resumed=updating.nav(true);
  expect(updating.steps).toEqual([]);
  expect(resumed.view()).toEqual(settings);
  resumed.back();
  expect(updating.steps).toEqual([-1]);
});

it('pushes actual selections, restores a popped entry and gives a new branch its own key',()=>{
  const h=fixture(),nav=h.nav();
  expect(nav.push(channel)).toBe(true);
  const first=h.port.state;
  nav.push(channel);nav.push(thread);
  const before=nav.entryKey();
  expect(h.pushed.map(v=>readEntry(v)?.depth)).toEqual([1,2]);
  nav.back();expect(h.steps).toEqual([-1]);
  h.pop(nav,first);
  expect(h.restored).toEqual([channel]);
  expect(h.pushed).toHaveLength(2);
  nav.push(channel,'m10'); // Open a linked post in the same conversation at a different anchor.
  expect(h.pushed.map(v=>readEntry(v)?.depth)).toEqual([1,2,2]);
  expect(nav.entryKey()).not.toBe(before);
});

it('dedupes the same linked post, including after Back, but pushes a different post',()=>{
  const h=fixture(),nav=h.nav();
  nav.push(channel,'m9');
  const first=h.port.state;
  expect(nav.push(channel,'m9')).toBe(true);
  expect(h.pushed).toEqual([first]);
  nav.push(channel,'m10');
  expect(h.pushed.map(v=>readEntry(v)?.depth)).toEqual([1,2]);
  h.pop(nav,first);
  expect(h.restored).toEqual([channel]);
  expect(nav.push(channel,'m9')).toBe(true);
  expect(h.pushed).toHaveLength(2);
});

it('still dedupes an ordinary tap on the current channel after a linked open',()=>{
  const h=fixture(),nav=h.nav();
  nav.push(channel,'m9');
  expect(nav.push(channel)).toBe(true);
  expect(h.pushed).toHaveLength(1);
});

it('rewinds only a recognized app run on reload, never reviving its Forward view',()=>{
  const h=fixture(entry(thread,2)),nav=h.nav();
  expect(h.steps).toEqual([-2]);expect(nav.push(channel)).toBe(false);
  h.pop(nav,entry(home));
  const root=h.port.state;
  expect(readEntry(root)).toMatchObject({view:home,run:'new',depth:0});
  h.pop(nav,entry(channel,1));
  expect(h.restored.every(v=>v.pane==='home')).toBe(true);
  expect(h.steps).toEqual([-2,-1]);
  h.pop(nav,root);
  nav.back();expect(h.steps).toEqual([-2,-1]); // App button never intercepts root Back.
});

it.each([['foreign state',{view:thread}],['an older run',entry(thread,2)]])('reanchors after a pop to %s so the next tap works',(_label,value)=>{
  const h=fixture(),nav=h.nav();
  nav.push(channel);
  h.pop(nav,value);
  expect(h.restored).toEqual([home]);
  expect(h.steps).toEqual([]); // No rewind by an unrecognized depth.
  expect(h.pushed).toHaveLength(1); // Restore itself never pushes.
  expect(nav.push(channel)).toBe(true);
  expect(h.pushed.map(v=>readEntry(v)?.depth)).toEqual([1,1]);
});

it('reanchors if the reload rewind lands on unknown state',()=>{
  const h=fixture(entry(thread,2)),nav=h.nav();
  expect(h.steps).toEqual([-2]);
  h.pop(nav,null);
  expect(h.restored).toEqual([home]);
  expect(h.pushed).toEqual([]);
  expect(nav.push(channel)).toBe(true);
  expect(readEntry(h.port.state)).toMatchObject({run:'new',depth:1,view:channel});
});

it('reanchors at the earliest remaining app entry after the browser prunes Home',()=>{
  const h=fixture(entry(thread,48),2),nav=h.nav();
  expect(h.steps).toEqual([-2]);
  expect(nav.push(channel)).toBe(false);
  h.pop(nav,entry(channel,46));
  expect(readEntry(h.port.state)).toMatchObject({run:'new',depth:0,view:home});
  expect(nav.push(channel)).toBe(true);
  expect(readEntry(h.port.state)?.depth).toBe(1);
});

it('keeps the existing Back position for an explicitly resumed worker update',()=>{
  const h=fixture(entry(thread,2)),nav=h.nav(true);
  expect(h.steps).toEqual([]);
  nav.back();expect(h.steps).toEqual([-1]);
  h.pop(nav,entry(channel,1));
  expect(h.restored).toEqual([channel]);
});

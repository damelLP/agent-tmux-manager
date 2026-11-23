import { expect, it } from 'vitest';
import { allowedOptions, cards, initial, reduce, roots, scopeKey } from '../src/state.ts';
import type { ChannelRow, DecisionRow, PostRow } from '../src/wire.ts';
import { decodeResult } from '../src/wire.ts';
import { captured } from './fixtures/atmd-aefd6014.ts';
import * as state from '../src/state.ts';

const channel: ChannelRow = {id:'demo',kind:'project',section:'channel',members:[{address:'owner'}],former:[],writable:true,activity_seq:11,unread:7};
const question: DecisionRow = {id:'d8',post_id:'m9',channel:'demo',status:'pending',request:{requester:'demo/lead',title:'Ready?',context:'',options:['Yes','Not yet']}};
const row: PostRow = {id:'m9',channel:'demo',author:'demo/lead',kind:'question',body:'Ready?',posted_at:'2026-01-01T12:00:00Z',attachments:[],mentions:[],replies:0};

it('shows open questions with or without a conversation, regardless of channel unread', () => {
  const a = reduce(initial(),{type:'channels',seq:11,rows:[channel]});
  const open=decodeResult('decision.list',captured.decisionLastPage.result)?.decisions ?? [];
  const b = reduce(a,{type:'decisions',rows:[question,...open,{...question,id:'d11',status:'resolved'}]});
  expect(cards(b).map(x=>x.id)).toEqual(['d8','d28']);
  expect(b.channels[0]?.unread).toBe(7);
  expect(allowedOptions(question)).toEqual(['Yes','Not yet']);
  expect(allowedOptions({...question,status:'taken_provisionally',provisional:{option:'Not yet'}})).toEqual(['Yes','Not yet']);
  expect(allowedOptions({...question,status:'taken_provisionally'})).toEqual([]);
  expect(allowedOptions({...question,status:'taken_provisionally',provisional:{option:'Other'}})).toEqual([]);
  expect(allowedOptions({...question,status:'expired'})).toEqual([]);
});

it.each([true,false])('keeps Settings when the selected channel is gone (replacement: %s)',replacement=>{
  const selected={...initial(),pane:'settings',channel:'demo',channels:[channel]} as const;
  const rows=replacement?[{...channel,id:'new'}]:[];
  const s=reduce({...selected,channels:[channel]}, {type:'channels',seq:12,rows});
  const want={pane:'settings',channel:replacement?'new':undefined,thread:undefined};
  expect(s).toMatchObject({pane:want.pane,channel:want.channel});
  expect(state.restoreView(s,selected)).toEqual(want);
});

it('does not monitor a DM peer in Settings',()=>{
  const id='demo/lead+owner';
  const dm:ChannelRow={...channel,id,kind:'direct',section:'lead',members:[{address:'demo/lead'},{address:'owner'}]};
  const view={...initial(),channel:id,channels:[dm]};
  expect(state.watchSeats({...view,pane:'channel'})).toEqual(['demo/lead']);
  expect(state.watchSeats({...view,pane:'settings'})).toEqual([]);
});

it('retains all options of a real provisional decision with a valid pick', () => {
  const row=decodeResult('decision.list',captured.decisionProvisional.result)?.decisions.at(0);
  if(!row) throw new Error('missing provisional fixture');
  expect(row.status).toBe('taken_provisionally');
  expect(allowedOptions(row)).toEqual(['First sample','Second sample']);
});

it('retains newer reply metadata and chronological IDs across overlapping pages', () => {
  const key = scopeKey('demo');
  const a = reduce(initial(),{type:'history',key,seq:20,rows:[{...row,replies:2}],older:true});
  const b = reduce(a,{type:'history',key,seq:19,rows:[{...row,id:'m10'},row]});
  expect(roots(b,key).map(x=>[x.id,x.replies])).toEqual([['m9',2],['m10',0]]);
  expect(b.histories.get(key)?.older).toBe(true);
});

it('highlights resolved incoming owner mentions, not own posts or literal text',()=>{
  const tagged:PostRow={...row,body:'Please check @owner',mentions:['owner']};
  const dm={...tagged,channel:'demo/lead+owner'};
  const variants:PostRow[]=[tagged,dm,{...dm,author:'owner'},{...dm,body:'A plain DM',mentions:[]},
    {...dm,body:'Literal `@owner`',mentions:[]},{...row,mentions:[{seat:'demo/owner'}]}];
  expect(variants.map(post=>state.incomingOwnerMention(post))).toEqual([true,true,false,false,false,false]);
});

it('shows a shared DM reply once, preferring the main projection',()=>{
  const shared={...row,id:'m12',thread:'m9',shared:true} as const;
  const plain={...row,id:'m10',thread:'m9'} as const;
  let s=state.reduce(state.initial(),{type:'history',seq:12,key:state.scopeKey('demo'),rows:[row,shared]});
  s=state.reduce(s,{type:'history',seq:13,key:state.scopeKey('demo','m9'),rows:[plain,{...shared,shared:undefined}]});
  expect(state.timeline(s,{...channel,kind:'direct'})).toEqual([row,plain,shared]);
});

it('merges DM replies by numeric ID only within their loaded root scopes',()=>{
  const root={...row,replies:2}, other={...row,id:'m11' as const};
  const put=(s:state.ChatState,key:string,rows:PostRow[])=>reduce(s,{type:'history',seq:30,key,rows});
  let s=put(initial(),scopeKey('demo'),[other,root]);
  s=put(s,scopeKey('demo','m9'),[{...row,id:'m12',thread:'m9'},{...row,id:'m10',thread:'m9'}]);
  s=put(s,scopeKey('demo','m3'),[{...row,id:'m13',thread:'m3'}]);
  s=put(s,scopeKey('foreign','m9'),[{...row,id:'m14',channel:'foreign',thread:'m9'}]);
  expect(state.timeline?.(s,{...channel,kind:'direct'}).map(r=>r.id)).toEqual(['m9','m10','m11','m12']);
  expect(state.timeline(s,channel).map(r=>r.id)).toEqual(['m9','m11']);
});

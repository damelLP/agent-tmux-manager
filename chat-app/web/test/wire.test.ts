import { describe, expect, it } from 'vitest';
import { beforeId, bodyError, connectFrame, daemonDown, decodeResult, parseFrame, pingFrame, refusal, requestFrame, watchFrame, wsUrl } from '../src/wire.ts';
import { captured } from './fixtures/atmd-aefd6014.ts';

const version = {major: 1, minor: 1};
const post = {id:'m10', channel:'demo', thread:null, author:'demo/lead', kind:'report', body:'Status',
  posted_at:'2026-01-01T12:00:00Z', attachments:['/private/notes.txt', 'C:\\private\\review.md'], replies:0,
  mentions:[{seat:'demo/lead'}], decision:null, legacy:true, routing:'router', woke:['owner']};

describe('ATM browser frames', () => {
  it('writes literal request envelopes without caller attribution', () => {
    expect(JSON.parse(requestFrame('r4','channel.seen',{channel:'demo',through:'m9'})))
      .toEqual({type:'request',protocol_version:version,id:'r4',method:'channel.seen',params:{channel:'demo',through:'m9'}});
    expect(JSON.parse(connectFrame())).toEqual({type:'connect',protocol_version:version});
    expect(JSON.parse(pingFrame(4))).toEqual({type:'ping',protocol_version:version,seq:4});
    expect(JSON.parse(watchFrame(9))).toEqual({type:'watch',protocol_version:version,cursor:9,topics:['channel.','decision.','msg.']});
    expect(wsUrl(new URL('https://example.invalid:443/'))).toBe('wss://example.invalid/ws');
  });
  it('rejects binary, mixed response and unsafe cursors without decoding event contents', () => {
    expect(parseFrame(new Uint8Array())).toBeNull();
    expect(parseFrame('{')).toBeNull();
    expect(parseFrame('{"type":"response","id":"a","result":null,"error":{"code":"refused","message":"x"}}')).toBeNull();
    expect(parseFrame('{"type":"event","seq":9007199254740993,"kind":"channel.posted"}')).toBeNull();
    expect(parseFrame('{"type":"event","seq":17,"kind":"msg.sent","body":{"x":1}}')).toEqual({type:'event',seq:17,kind:'msg.sent'});
  });
  it('normalizes optional fields and retains live attachment paths', () => {
    const page = decodeResult('channel.history',{seq:19,channel:'demo',posts:[post],more:true});
    expect(page?.posts).toEqual([{id:'m10',channel:'demo',author:'demo/lead',kind:'report',body:'Status',
      posted_at:post.posted_at,attachments:post.attachments,replies:0,mentions:[{seat:'demo/lead'}]}]);
    expect(decodeResult('channel.history',{seq:19,channel:'demo',posts:[{...post,id:'m01'}],more:false})).toBeNull();
    expect(decodeResult('channel.history',{seq:NaN,channel:'demo',posts:[],more:false})).toBeNull();
    expect(beforeId('m9','m10')).toBe(true);
    expect(beforeId('m9007199254740993','m9007199254740994')).toBe(true);
  });
  it('keeps the edit/remove target without trusting or retaining other watch body fields',()=>{
    const frame=(kind:string,body:unknown)=>parseFrame(JSON.stringify({type:'event',kind,seq:25,body}));
    expect(frame('channel.post_edited',{post:'m10',actor:'owner',body:'untrusted'}))
      .toEqual({type:'event',kind:'channel.post_edited',seq:25,post:'m10'});
    expect(frame('channel.post_removed',{post:'m01'})).toEqual({type:'event',kind:'channel.post_removed',seq:25});
    expect(frame('channel.posted',{post:'m10'})).toEqual({type:'event',kind:'channel.posted',seq:25});
  });
  it('decodes additive edit/remove history flags and canonical receipts',()=>{
    const history=(extra:object)=>decodeResult('channel.history',{seq:25,channel:'demo',posts:[{...post,legacy:false,...extra}],more:false});
    expect(history({body:'New copy',edited:true})?.posts.at(0)).toMatchObject({body:'New copy',edited:true,native:true});
    expect(history({body:'removed',removed:true,attachments:[],mentions:[]})?.posts.at(0)).toMatchObject({removed:true,body:'removed'});
    expect(history({edited:'yes'})).toBeNull();
    expect(history({removed:0})).toBeNull();
    for(const method of ['channel.edit','channel.remove'] as const) {
      expect(decodeResult(method,{seq:25,post_id:'m10'})).toEqual({seq:25,post_id:'m10'});
      expect(decodeResult(method,{seq:25,post:'m10'})).toBeNull();
    }
  });
  it('projects fixed-order reaction counts and only the watch target',()=>{
    const reactions=[{emoji:'👍',count:2,mine:false},{emoji:'👀',count:1,mine:true}];
    const history=(value:unknown)=>decodeResult('channel.history',{seq:25,channel:'demo',posts:[{...post,legacy:false,reactions:value}],more:false});
    expect(history(reactions)?.posts.at(0)?.reactions).toEqual(reactions);
    expect(history([])?.posts.at(0)).not.toHaveProperty('reactions');
    for(const malformed of [[reactions[1],reactions[0]],[reactions[0],reactions[0]],
      [{emoji:'<img>',count:1,mine:false}],[{emoji:'👍',count:-1,mine:false}],[{emoji:'👍',count:2,mine:1}]]) expect(history(malformed)).toBeNull();
    expect(parseFrame(JSON.stringify({type:'event',kind:'channel.reacted',seq:25,body:{post:'m10',emoji:'👍',actor:'owner',active:true}})))
      .toEqual({type:'event',kind:'channel.reacted',seq:25,post:'m10'});
    expect(decodeResult('channel.react',{seq:25,post_id:'m10'})).toEqual({seq:25,post_id:'m10'});
    expect(decodeResult('channel.react',{seq:25,post_id:'m01'})).toBeNull();
  });
  it('projects real Connected, Pong and Watch frames without event contents', () => {
    const connected = parseFrame(JSON.stringify(captured.connected));
    expect(connected).toMatchObject({type:'connected',methods:expect.arrayContaining([
      'channel.list','channel.history','channel.post','channel.seen','decision.list','decision.resolve',
    ])});
    expect(parseFrame(JSON.stringify(captured.pong))).toEqual({type:'pong',seq:captured.pong.seq});
    expect(parseFrame(JSON.stringify(captured.posted))).toEqual({type:'event',seq:11,kind:'channel.posted'});
  });
  it('preserves real list addresses, former seats, unread markers and sections', () => {
    const list=decodeResult('channel.list',captured.list.result);
    expect(list?.channels.map(c=>[c.id,c.section,c.writable])).toEqual([
      ['demo','channel',true],['demo/chief+owner','chief',true],['demo/lead+demo/worker','direct',false],
      ['demo/lead+owner','lead',true],['owner+system','system',true],
    ]);
    expect(list?.channels[0]).toMatchObject({members:[{address:'owner'},{address:'demo/lead'},{address:'demo/worker'}],
      former:['demo/chief'],seen_through:'m194',unread:1,activity_seq:204});
  });
  it('projects a bare agent.list array without retaining per-seat metadata', () => {
    const extra = {pane:'%10',session_id:'fixture-session',harness:'pi',model:'fixture',cost_usd:0,reason:null};
    expect(decodeResult('agent.list',[
      {agent_id:'demo/a',state:'ready',activity:'working',...extra},
      {agent_id:'demo/b',state:'ready',activity:'attention_needed',...extra},
      {agent_id:'demo/c',state:'ready',activity:'idle',...extra},
      {agent_id:'demo/d',state:'down',activity:null,...extra},
    ])).toEqual([
      {agent_id:'demo/a',state:'ready',activity:'working'},
      {agent_id:'demo/b',state:'ready',activity:'attention_needed'},
      {agent_id:'demo/c',state:'ready',activity:'idle'},
      {agent_id:'demo/d',state:'down',activity:null},
    ]);
    expect(decodeResult('agent.list',[])).toEqual([]);
  });
  it('rejects an unexpected agent.list envelope and malformed rows as a failed read', () => {
    const row = {agent_id:'demo/a',state:'ready',activity:'working'};
    for (const value of [null,{agents:[row]},[row,{}],[{...row,agent_id:7}],[{...row,state:''}],
      [{...row,activity:undefined}],[{...row,activity:'busy'}]]) {
      expect(decodeResult('agent.list',value)).toBeNull();
    }
  });
  it('retains the opt-in bit on a reply but not on a root or a malformed flag', () => {
    const decode=(extra:object)=>decodeResult('channel.history',{seq:19,channel:'demo',posts:[{...post,...extra}],more:false});
    expect(decode({thread:'m9',also_in_chat:true})?.posts.at(0)).toMatchObject({id:'m10',thread:'m9',shared:true});
    expect(decode({also_in_chat:true})).toBeNull();
    expect(decode({thread:'m10',also_in_chat:true})).toBeNull();
    expect(decode({thread:'m9',also_in_chat:'true'})).toBeNull();
    expect(decode({thread:'m9',also_in_chat:false})?.posts.at(0)).not.toHaveProperty('shared');
  });

  it('decodes real thread pages and legacy history without leaking wire-only fields', () => {
    const first=decodeResult('channel.history',captured.thread.result);
    const after=decodeResult('channel.history',captured.threadAfter.result);
    const legacy=decodeResult('channel.history',captured.legacy.result);
    expect(first).toMatchObject({channel:'demo',more:true,posts:[
      {id:'m11',attachments:['summary.txt'],mentions:['owner'],replies:2},{id:'m21',thread:'m11',author:'owner'},
    ]});
    expect(first?.posts[0]).not.toHaveProperty('thread');
    expect(first?.posts[0]).not.toHaveProperty('client_key');
    expect(after).toMatchObject({more:false,posts:[{id:'m24',thread:'m11'}]});
    expect(legacy?.posts[0]).toMatchObject({id:'m209',author:'system',channel:'owner+system'});
  });
  it('projects canonical post IDs across routing changes and acknowledges repeated seen', () => {
    expect(decodeResult('channel.post',captured.post.result)).toEqual({post_id:'m18',channel:'demo'});
    expect(decodeResult('channel.post',captured.postReplay.result)).toEqual({post_id:'m18',channel:'demo',
      feedback:{woke:['demo/lead'],unavailable:[],unresolved:[]}});
    expect(decodeResult('channel.post',captured.reply.result)).toEqual({post_id:'m21',channel:'demo',thread:'m11'});
    expect(decodeResult('channel.seen',captured.seen.result)).toEqual({changed:true,through:'m11'});
    expect(decodeResult('channel.seen',captured.seenAgain.result)).toEqual({changed:false,through:'m11'});
    expect(parseFrame(JSON.stringify(captured.postUnknownChannel))).toMatchObject({error:'unknown_channel'});
  });
  it('decodes wake feedback without invalidating a legacy ack or an ack with malformed extras',()=>{
    // Captured: atmd 0.5.2 (dc113e66), 2026-11-21, sealed Unix RPC with inert seats.
    const receipt={channel:'demo',mentions:[{seat:'demo/lead'},{seat:'demo/offline'}],post_id:'m5',route:'none',seq:7,thread:null,
      unavailable:[{message_id:'m7',since:'2026-11-21T13:00:21.560988838Z',state:'down',to:'demo/offline'}],
      unresolved:[{reason:'unknown',text:'@ghost'}],woke:[{message_id:'m6',to:'demo/lead'},{message_id:'m7',to:'demo/offline'}]};
    expect(decodeResult('channel.post',receipt)).toEqual({post_id:'m5',channel:'demo',
      feedback:{woke:['demo/lead','demo/offline'],unavailable:['demo/offline'],unresolved:['@ghost']}});
    expect(decodeResult('channel.post',{...receipt,woke:[null,{to:7},{to:'demo/lead'}],unavailable:false,unresolved:[{text:''}]}))
      .toEqual({post_id:'m5',channel:'demo',feedback:{woke:['demo/lead'],unavailable:[],unresolved:[]}});
    expect(decodeResult('channel.post',{post_id:'m5',channel:'demo',woke:false,unavailable:[{}]})).toEqual({post_id:'m5',channel:'demo'});
    expect(decodeResult('channel.post',{...receipt,post_id:'bad'})).toBeNull();
  });
  it('projects flat, unlinked real decisions and preserves optional conversation links', () => {
    const first=decodeResult('decision.list',captured.decisionFirstPage.result);
    const last=decodeResult('decision.list',captured.decisionLastPage.result);
    expect(first?.next_after).toBe('d27');
    expect(last?.next_after).toBeUndefined();
    expect(last?.decisions).toEqual([{id:'d28',status:'pending',request:{requester:'demo/worker',title:'Second survey decision',
      context:'Only synthetic material',options:['First sample','Second sample']}}]);
    // Synthetic: an additive U8 link, absent from the captured pre-U8 row.
    const linked={...captured.decisionLastPage.result.decisions[0],channel:'demo',post_id:'m42'};
    expect(decodeResult('decision.list',{decisions:[linked]})?.decisions[0]).toMatchObject({channel:'demo',post_id:'m42'});
    for(const [frame,changed] of [[captured.resolve,true],[captured.resolveAgain,false]] as const)
      expect(decodeResult('decision.resolve',frame.result)).toMatchObject({changed,decision:{id:'d27',status:'resolved'}});
  });
  it('encodes optional withdrawal reasons and validates mutation receipts', () => {
    expect(JSON.parse(requestFrame('r1','decision.withdraw',{id:'d8'}))).toEqual({
      type:'request',protocol_version:version,id:'r1',method:'decision.withdraw',params:{id:'d8'},
    });
    expect(JSON.parse(requestFrame('r2','decision.withdraw',{id:'d8',reason:'Fixture is stale'})).params)
      .toEqual({id:'d8',reason:'Fixture is stale'});
    const decision={...captured.resolve.result.decision,status:'withdrawn',resolution:null};
    expect(decodeResult('decision.withdraw',{decision,changed:true})).toMatchObject({changed:true,decision:{id:'d27',status:'withdrawn'}});
    expect(decodeResult('decision.withdraw',{decision:{...decision,post_id:'m42',channel:'demo'},changed:false}))
      .toMatchObject({changed:false,decision:{post_id:'m42',channel:'demo'}});
    expect(decodeResult('decision.withdraw',{decision})).toBeNull();
    expect(decodeResult('decision.withdraw',{decision:{...decision,id:'bogus'},changed:true})).toBeNull();
    expect(decodeResult('decision.resolve',captured.resolve.result)?.option).toBe('First sample');
  });
  it('distinguishes real W0 diagnostic JSON from untyped refusal', () => {
    expect(daemonDown(captured.healthDown.body)).toBe(true);
    expect(refusal(captured.originDenied.body)).toBe('origin');
    expect(refusal(captured.identityDenied.body)).toBe('identity');
    expect(refusal({})).toBeNull();
  });
  it('counts UTF-8 bytes without stripping meaningful whitespace', () => {
    expect(bodyError(' \n')).toBe('blank');
    expect(bodyError(' \n',true)).toBeNull();
    expect(bodyError(' '.repeat(16_385),true)).toBe('long');
    expect(bodyError('é'.repeat(8192))).toBeNull();
    expect(bodyError('é'.repeat(8192)+'z')).toBe('long');
  });
});

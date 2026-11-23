import { WebSocketServer } from 'ws';
import { methods, record, version } from '../src/wire.ts';

export function diagnostics(req,res,options={}) {
  if (req.url === '/health' && req.method === 'GET') {
    const health=options.health?.() ?? 'up';
    if (health === 'identity') res.writeHead(403,{'Content-Type':'application/json'}).end('{"error":"identity"}');
    else if (health === 'absent') res.writeHead(404).end();
    else res.writeHead(200,{'Content-Type':'application/json','Cache-Control':'no-store'}).end(JSON.stringify({bridge:'ok',daemon:health,protocol:'1.1'}));
    return true;
  }
  if (req.url === '/ws' && req.method === 'POST') {
    const error=options.refusal?.() || (req.headers.origin === `http://${req.headers.host}` ? '' : 'origin');
    if (error) res.writeHead(403,{'Content-Type':'application/json'}).end(JSON.stringify({error}));
    else res.writeHead(405).end();
    return true;
  }
  return false;
}

export function attachFake(server,fake,options={}) {
  const wss=new WebSocketServer({noServer:true,maxPayload:1_048_575});
  function upgrade(req,socket,head) {
    if (req.url !== '/ws') return;
    if (options.blocked?.() || options.refusal?.() || (req.headers.origin && req.headers.origin !== `http://${req.headers.host}`)) {
      socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n'); return;
    }
    wss.handleUpgrade(req,socket,head,stream=>wss.emit('connection',stream));
  }
  server.on('upgrade',upgrade);
  wss.on('connection',ws=>{
    let ready=false, cancel, supported=methods;
    const send=f=>{if(ws.readyState===1) ws.send(JSON.stringify(f));};
    ws.on('error',()=>ws.terminate());
    ws.on('close',()=>cancel?.());
    ws.on('message',(data,binary)=>{
      let f;
      try { f=!binary && record(JSON.parse(data.toString())); } catch { /* malformed synthetic client frame */ }
      if (!f || f.protocol_version?.major !== 1 || f.protocol_version?.minor !== 1) { ws.close(1002); return; }
      options.frame?.(f);
      if (!ready) {
        if (f.type !== 'connect') { ws.close(1002); return; }
        if (fake.hasActivity()) supported=[...methods,'agent.list'];
        ready=true; send({type:'connected',protocol_version:version,client_id:'demo-client',methods:supported}); return;
      }
      if (f.type === 'ping' && Number.isSafeInteger(f.seq)) send({type:'pong',seq:f.seq});
      else if (f.type === 'watch' && Number.isSafeInteger(f.cursor) && JSON.stringify(f.topics) === '["channel.","decision.","msg."]') {
        cancel?.(); cancel=fake.watch(f.cursor,send);
      } else if (f.type === 'request' && typeof f.id === 'string') {
        const result=supported.includes(f.method) && !('from_pane' in f) ? fake.call(f.method,f.params) : {error:{code:'refused',message:'Synthetic request refused'}};
        if (options.drop?.(f,result)) { ws.terminate(); return; }
        send({type:'response',id:f.id,...('error' in result ? result : {result})});
      } else send({type:'error',code:'refused',message:'Synthetic frame refused'});
    });
  });
  const stop=()=>{ server.off('upgrade',upgrade); stop.disconnect(); wss.close(); };
  stop.disconnect=()=>{ for (const ws of wss.clients) ws.terminate(); };
  return stop;
}

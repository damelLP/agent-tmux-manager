// Loopback-only test stand-in for Tailscale Serve, NOT a deployable proxy.
// Set the fixture identity on every HTTP request and Upgrade. Do not touch Origin
// or any other header. POST /upload alone can carry a Content-Length body.
import {createServer} from 'node:tls';
import {createConnection} from 'node:net';
import {once} from 'node:events';

const limit=32*1024, delimiter='\r\n\r\n';
function requestHead(bytes,login) {
  const [line,...headers]=bytes.toString('latin1').split('\r\n');
  if(!/^(GET|HEAD|POST) \S+ HTTP\/1\.1$/.test(line)) throw new Error('Unsupported fixture request');
  let upgrade=false, length;
  const kept=headers.filter(value=>{
    const match=/^([\w-]+):[ \t]*(.*)$/.exec(value);
    if(!match) throw new Error('Invalid fixture header');
    const name=match[1].toLowerCase(), body=match[2].trim().toLowerCase();
    if(name==='transfer-encoding') throw new Error('Unsupported fixture transfer-encoding');
    if(name==='content-length') {
      if(length!==undefined||!/^\d+$/.test(body)) throw new Error('Invalid fixture length');
      length=Number(body);
    }
    if(name==='upgrade'&&body==='websocket') upgrade=true;
    return name!=='tailscale-user-login';
  });
  if(length && (!line.startsWith('POST /upload?') || length>32*1024*1024)) throw new Error('Unsupported fixture request body');
  return {upgrade,length:length??0,wire:Buffer.from([line,...kept,`Tailscale-User-Login: ${login}`].join('\r\n')+delimiter,'latin1')};
}

export async function serveHop(credential,target,identity) {
  const clients=new Set();
  const server=createServer({...credential,ALPNProtocols:['http/1.1']},down=>{
    const port=target();
    if(!port) {down.destroy();return;}
    const up=createConnection(port,'127.0.0.1');
    clients.add(down);
    down.once('close',()=>{clients.delete(down);up.destroy();});
    down.on('error',()=>up.destroy());
    up.once('close',()=>down.destroy());up.on('error',()=>down.destroy());
    up.pipe(down);
    let head=Buffer.alloc(0), remaining=0;
    down.on('data',receive);
    function receive(chunk) {
      head=Buffer.concat([head,chunk]);
      for(;;) {
        if(remaining) {
          const count=Math.min(head.length,remaining);
          if(count&&!up.write(head.subarray(0,count))) {down.pause();up.once('drain',()=>down.resume());}
          remaining-=count;head=head.subarray(count);
          if(remaining) return;
        }
        const end=head.indexOf(delimiter);
        if(end<0) {if(head.length>limit)down.destroy();return;}
        if(end>limit) {down.destroy();return;}
        try {
          const login=identity();
          if(!/^[\x21-\x7e]+$/.test(login)) throw new Error('Invalid fixture login');
          const request=requestHead(head.subarray(0,end),login);
          up.write(request.wire);head=head.subarray(end+delimiter.length);remaining=request.length;
          if(request.upgrade) {
            down.off('data',receive);up.write(head);head=Buffer.alloc(0);down.pipe(up);return;
          }
        } catch {down.destroy();return;}
      }
    }
  });
  const ready=once(server,'listening');server.listen(0,'127.0.0.1');await ready;
  return {origin:`https://127.0.0.1:${server.address().port}`,
    async close(){const done=once(server,'close');for(const c of clients)c.destroy();server.close();await done;}};
}

import { createServer } from 'node:http';
import { once } from 'node:events';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createFake } from '../../dev/fake.ts';
import { attachFake, diagnostics } from '../../dev/server.mjs';

const mime={html:'text/html',js:'text/javascript',css:'text/css',json:'application/json',webmanifest:'application/json',png:'image/png',woff2:'font/woff2'};
function assets(dir) {
  const data=new Map();
  for(const prefix of ['','assets/']) {
    try {
      for(const entry of readdirSync(join(dir,prefix),{withFileTypes:true})) {
        if(entry.isFile()) data.set('/'+prefix+entry.name,readFileSync(join(dir,prefix,entry.name)));
      }
    } catch(e) { if(prefix==='' || e.code!=='ENOENT') throw e; }
  }
  return data;
}
export async function rig(dir) {
  let files=assets(dir);
  const control={blocked:false,health:'up',refusal:'',omit:null};
  const fake=createFake(), frames=[], http=[];
  const options={blocked:()=>control.blocked,health:()=>control.health,refusal:()=>control.refusal,frame:f=>frames.push(f),
    drop:(f,result)=>control.drop?.(f,result)};
  const server=createServer((req,res)=>{
    http.push({method:req.method,url:req.url});
    if(control.http?.(req,res)||diagnostics(req,res,options)) return;
    const path=new URL(req.url,'http://127.0.0.1').pathname, file=path==='/'?'/index.html':path;
    const body=control.omit===file?undefined:files.get(file);
    if(req.method!=='GET'||!body) {res.writeHead(404).end(); return;}
    res.writeHead(200,{'Content-Type':mime[file.split('.').at(-1)]??'application/octet-stream','Cache-Control':'no-store','X-Content-Type-Options':'nosniff'}).end(body);
  });
  const stop=attachFake(server,fake,options);
  const ready=once(server,'listening'); server.listen(0,'127.0.0.1'); await ready;
  return {origin:`http://127.0.0.1:${server.address().port}`,control,fake,frames,http,disconnect:stop.disconnect,
    publish(next){files=assets(next);}, async close(){stop(); server.closeAllConnections(); await new Promise(r=>server.close(()=>r()));}};
}

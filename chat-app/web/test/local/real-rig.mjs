import assert from 'node:assert/strict';
import {execFileSync,spawn} from 'node:child_process';
import {createHash,X509Certificate} from 'node:crypto';
import {EventEmitter,once} from 'node:events';
import {mkdirSync,readFileSync,readlinkSync,realpathSync,writeFileSync} from 'node:fs';
import {join} from 'node:path';
import {createInterface} from 'node:readline';
import WebSocket from 'ws';
import {serveHop} from './serve-hop.mjs';

export const topics=['channel.','decision.','msg.'];
export function journal() {
  const rows=[], events=new EventEmitter();
  return {rows, add(row){rows.push(row);events.emit('row',row);}, async next(test,from=0) {
    const found=rows.slice(from).find(test);
    if(found) return found;
    return new Promise(resolve=>{
      const check=row=>{if(test(row)){events.off('row',check);resolve(row);}};
      events.on('row',check);
    });
  }};
}

const command=(exe,args,env=process.env)=>execFileSync(exe,args,{env,encoding:'utf8',timeout:45_000,stdio:['ignore','pipe','pipe']}).trim();
function start(exe,args,pattern) {
  const child=spawn(exe,args,{env:{...process.env,RUST_LOG:'info'},stdio:['ignore','pipe','pipe']});
  const out=[];
  const exit=once(child,'exit');void exit.catch(()=>{});
  const ready=new Promise((resolve,reject)=>{
    child.once('error',reject);
    child.once('exit',(code,signal)=>reject(new Error(`${exe} exited (${code ?? signal}):\n${out.join('\n')}`)));
    for (const stream of [child.stdout,child.stderr]) createInterface({input:stream}).on('line',line=>{
      out.push(line);if(out.length>30) out.shift();
      const match=pattern.exec(line);if(match)resolve(match);
    });
  });
  return {ready,async stop(){
    if(child.exitCode===null&&child.signalCode===null) {
      assert.equal(readlinkSync(`/proc/${child.pid}/exe`),exe,'only signal the child binary we started');
      child.kill('SIGINT');
    }
    const [code]=await exit;
    assert.equal(code,0,`${exe} failed: ${out.join('\n')}`);
  }};
}

export async function realRig([atmd,atm,bridge], {files=false}={}) {
  const root=process.env.ATM_CHAT_TEST_RUN, socket=process.env.ATM_SOCKET;
  assert.ok(process.pid===1&&root&&socket===join(root,'s/atm.sock')&&!process.env.TMUX&&!process.env.TMUX_PANE,
    'run --real through test/local/seal-real.sh');
  assert.equal(process.env.TMUX_TMPDIR,join(root,'tmux'));
  [atmd,atm,bridge]=[atmd,atm,bridge].map(path=>realpathSync(path));
  const project=join(root,'demo'), roles=join(project,'.atm/roles'), store=join(root,'files');mkdirSync(roles,{recursive:true});
  const docs=join(project,'docs/plans');
  if(files) mkdirSync(docs,{recursive:true});
  const serveRoot=files?realpathSync(docs):undefined;
  writeFileSync(join(project,'.atm/away.toml'),'stacking_cap = 2\n');
  const tmux=(...args)=>command('/usr/bin/tmux',['-L',process.env.ATM_TMUX_SOCKET,'-f','/dev/null',...args]);
  const login='fixture-owner';
  let sentLogin=login, port=0, owner, daemon, relay;
  const panes={}, clients=new Set();
  const key=join(root,'config/key.pem'), cert=join(root,'config/cert.pem');
  command('/usr/bin/openssl',['req','-x509','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-nodes','-days','1',
    '-subj','/CN=127.0.0.1','-addext','subjectAltName=IP:127.0.0.1','-keyout',key,'-out',cert]);
  const credential={key:readFileSync(key),cert:readFileSync(cert)};
  const publicKey=new X509Certificate(credential.cert).publicKey.export({type:'spki',format:'der'});
  const certificatePin=createHash('sha256').update(publicKey).digest('base64');
  const hop=await serveHop(credential,()=>port,()=>sentLogin);
  const cli=(seat,...args)=>{
    const pane=seat&&panes[seat]; if(seat)assert.match(pane,/^%\d+$/);
    try {return command(atm,args,pane?{...process.env,TMUX_PANE:pane}:process.env);}
    catch(e){throw new Error(`sealed atm ${args[0]} ${args[1] ?? ''}: ${e.stderr ?? e.message}`,{cause:e});}
  };
  const rig={origin:hop.origin,certificatePin,login,store,serveRoot,setLogin(value){sentLogin=value;},cli,
    json:(seat,...args)=>JSON.parse(cli(seat,...args,'--format','json')),
    async startDaemon(){daemon=start(atmd,['start'],/Daemon server listening/);await daemon.ready;},
    async stopDaemon(){const d=daemon;daemon=undefined;await d?.stop();},
    async startBridge(origin=hop.origin){
      relay=start(bridge,['--socket',socket,'--owner-login',login,'--public-origin',origin,
        '--page-dir',process.env.ATM_CHAT_TEST_DIST,'--port','0',...(files?['--files-dir',store,'--serve-root',serveRoot]:[])],/^Local: http:\/\/127\.0\.0\.1:(\d+)\//);
      port=Number((await relay.ready)[1]);
    },
    async stopBridge(){const d=relay;port=0;relay=undefined;await d?.stop();},
    async wire(){
      const ws=new WebSocket(`ws://127.0.0.1:${port}/ws`,{origin:hop.origin,headers:{'Tailscale-User-Login':login}});
      const frames=journal(), pending=new Map();let serial=0;
      clients.add(ws);
      ws.on('message',data=>{
        const f=JSON.parse(String(data));frames.add(f);pending.get(f.id)?.resolve(f);
      });
      ws.on('error',error=>{for(const waiter of pending.values())waiter.reject(error);});
      ws.on('close',()=>{clients.delete(ws);for(const waiter of pending.values())waiter.reject(new Error('CLI oracle socket closed'));});
      await once(ws,'open');
      const send=(type,body={})=>ws.send(JSON.stringify({type,protocol_version:{major:1,minor:1},...body}));
      send('connect'); const connected=await frames.next(f=>f.type==='connected');
      return {frames,connected,watch(cursor){send('watch',{topics,cursor});},async rpc(method,params={}) {
        const id=`probe-${++serial}`, waiter=Promise.withResolvers();pending.set(id,waiter);
        try {
          send('request',{id,method,params});const f=await waiter.promise;
          assert.ok('result' in f,`${method}: ${JSON.stringify(f)}`);
          return f.result;
        } finally {pending.delete(id);}
      },async close(){if(ws.readyState===ws.OPEN){const done=once(ws,'close');ws.close();await done;}}};
    },
    async close(){
      for(const ws of clients)ws.terminate();
      try {await rig.stopBridge();} finally {
        try {await rig.stopDaemon();} finally {
          await hop.close();
          if(owner) {assert.equal(tmux('display-message','-p','#{socket_path} #{pid}'),owner);tmux('kill-server');}
        }
      }
    },
  };
  try {
    for(const seat of ['lead','worker','chief']) {
      writeFileSync(join(roles,`${seat}.md`),`+++\nharness = 'pi'\n${seat==='worker'?"manager = 'lead'\n":''}+++\nInert test pane.\n`);
      panes[seat]=tmux('new-session','-d','-s',seat,'-P','-F','#{pane_id}','-c',project,'/bin/sh');
      const id=tmux('display-message','-p','-t',panes[seat],'#{socket_path} #{pid}');
      assert.ok(id.startsWith(join(root,'tmux')+'/')&&/ \d+$/.test(id));
      owner??=id;assert.equal(id,owner);
    }
    await rig.startDaemon();
    for(const seat of Object.keys(panes)) assert.equal(rig.json(null,'agent','adopt',seat,panes[seat],'--project-root',project).agent_id,`demo/${seat}`);
    await rig.startBridge();
    return rig;
  } catch(e) {await rig.close();throw e;}
}

import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { gzipSync } from 'node:zlib';
import { expect, it } from 'vitest';

const runAudit=(dir:string)=>spawnSync(process.execPath,['scripts/check-size.mjs',dir],{encoding:'utf8'});
function withDist(check:(write:(name:string,value:string|Buffer)=>void,audit:()=>ReturnType<typeof runAudit>)=>void) {
  const dir=mkdtempSync(join(tmpdir(),'atm-size-'));
  const write=(name:string,value:string|Buffer)=>writeFileSync(join(dir,name),value);
  try {
    mkdirSync(join(dir,'assets'));
    for(const name of ['index.html','manifest.webmanifest','icon-192.png','icon-512.png','Atkinson-OFL.txt']) write(name,'fixture');
    check(write,()=>runAudit(dir));
  } finally { rmSync(dir,{recursive:true,force:true}); }
}

it('counts the worker and chunks and rejects the first raw byte over budget',()=>withDist((write,audit)=>{
  write('assets/index-a.js','a'.repeat(98*1024)); write('assets/index-b.css','c'.repeat(1024)); write('sw.js','b'.repeat(1024));
  expect(audit()).toMatchObject({status:0,stdout:expect.stringContaining('102400 raw bytes')});
  write('sw.js','b'.repeat(1025));
  expect(audit()).toMatchObject({status:1,stdout:expect.stringContaining('102401 raw bytes'),stderr:expect.stringContaining('100 KiB raw')});
}));

it('counts gzip per file and rejects the first gzip byte over budget',()=>withDist((write,audit)=>{
  const entropy=Buffer.concat(Array.from({length:1216},(_,i)=>createHash('sha256').update(String(i)).digest()));
  const worker='x', workerBytes=gzipSync(worker).length;
  // Pick exact-size inputs without assuming a fixed deflate block overhead.
  const fixture=(bytes:number)=>{
    for(let n=bytes-128;n<=bytes;n++) {
      const body=entropy.subarray(0,n);
      if(gzipSync(body).length+workerBytes===bytes) return body;
    }
    throw new Error(`Could not construct ${bytes} gzip bytes`);
  };
  write('sw.js',worker); write('assets/index-a.js',fixture(38_912));
  expect(audit()).toMatchObject({status:0,stdout:expect.stringContaining('38912 gzip bytes')});
  write('assets/index-a.js',fixture(38_913));
  expect(audit()).toMatchObject({status:1,stdout:expect.stringContaining('38913 gzip bytes'),stderr:expect.stringContaining('38 KiB gzip')});
}));

it('caps full dist at 148 KiB and rejects stray static files',()=>withDist((write,audit)=>{
  write('assets/index-a.js','x'); write('sw.js','x');
  const index='x'.repeat(151_552-4*'fixture'.length-2);
  write('index.html',index);
  expect(audit()).toMatchObject({status:0,stdout:expect.stringContaining('dist: 151552 raw bytes')});
  write('index.html',index+'x');
  expect(audit()).toMatchObject({status:1,stdout:expect.stringContaining('dist: 151553 raw bytes'),stderr:expect.stringContaining('148 KiB raw dist')});
  write('assets/dev-fixture.json','x');
  expect(audit().stderr).toMatch(/Unexpected asset/);
}));

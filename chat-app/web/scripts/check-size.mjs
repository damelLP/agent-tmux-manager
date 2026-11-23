import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';

const dir=process.argv.at(2) ?? 'dist';
const root=new Set(['index.html','sw.js','manifest.webmanifest','icon-192.png','icon-512.png','Atkinson-OFL.txt']);
let raw=0, gzip=0, total=0;
function count(path) {
  const bytes=readFileSync(join(dir,path));
  total+=bytes.length;
  if(/\.(js|css)$/.test(path)){raw+=bytes.length;gzip+=gzipSync(bytes).length;}
}
for(const entry of readdirSync(dir,{withFileTypes:true})) {
  if(entry.name==='assets' && entry.isDirectory()) {
    for(const file of readdirSync(join(dir,'assets'),{withFileTypes:true})) {
      if(!file.isFile()||!/^[\w-]+\.(js|css|woff2)$/.test(file.name)) throw new Error(`Unexpected asset: ${file.name}`);
      count(`assets/${file.name}`);
    }
  } else {
    if(!entry.isFile()||!root.delete(entry.name)) throw new Error(`Unexpected asset: ${entry.name}`);
    count(entry.name);
  }
}
if(root.size) throw new Error('Missing required shell asset');
console.log(`JS+CSS: ${raw} raw bytes / ${gzip} gzip bytes; dist: ${total} raw bytes`);
for(const [measured,kib,label] of [[raw,100,'raw JS+CSS'],[gzip,38,'gzip JS+CSS'],[total,148,'raw dist']]) {
  if(measured>kib*1024){console.error(`Exceeds ${kib} KiB ${label}`);process.exitCode=1;}
}

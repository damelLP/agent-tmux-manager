import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { transformWithEsbuild } from 'vite';

const dir=process.argv.at(2) ?? 'dist';
const template=readFileSync(new URL('../src/sw.js',import.meta.url),'utf8');
const paths=['/index.html','/manifest.webmanifest','/icon-192.png','/icon-512.png','/Atkinson-OFL.txt'];
for (const file of readdirSync(join(dir,'assets'),{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))) {
  if(!file.isFile() || !/^[\w-]+\.(css|js|woff2)$/.test(file.name)) throw new Error('Unexpected static asset');
  paths.push(`/assets/${file.name}`);
}
const hash=createHash('sha256').update(template);
for (const p of paths) hash.update(p).update(readFileSync(join(dir,p)));
const indexHash=createHash('sha256').update(readFileSync(join(dir,'index.html'))).digest('base64');
const shell=JSON.stringify([hash.digest('hex').slice(0,24),indexHash,paths]);
const {code}=await transformWithEsbuild(template.replace('/* @SHELL@ */ []',shell),'sw.js',{minify:true,target:'es2022'});
writeFileSync(join(dir,'sw.js'),code);

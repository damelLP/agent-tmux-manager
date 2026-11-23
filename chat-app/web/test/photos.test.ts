import { afterEach, expect, it, vi } from 'vitest';
import { fileDetails, fileHead, fileText, preparePick } from '../src/photos.ts';

afterEach(()=>vi.unstubAllGlobals());

it('refuses an undecodable Photos pick but preserves the same file when chosen through Files',async()=>{
  const file=new File(['original bytes'],'camera.heic',{type:'image/heic'});
  const decode=vi.fn().mockRejectedValue(new Error('no HEIC decoder'));
  vi.stubGlobal('createImageBitmap',decode);
  await expect(preparePick(file,true)).rejects.toThrow("Can't shrink this photo. Send it from Files to send it as is.");
  const pick=await preparePick(file,false);
  expect(pick).toEqual({blob:file,name:'camera.heic',photo:false});
  expect(pick.blob).toBe(file);
  expect(decode).toHaveBeenCalledTimes(1);
});

it('lets exactly 32 MiB through the Files path and refuses the next byte',async()=>{
  const limit=new File([new Uint8Array(32*1024*1024)],'file.bin');
  await expect(preparePick(limit,false)).resolves.toMatchObject({blob:limit});
  await expect(preparePick(new File([limit,'x'],'file.bin'),false)).rejects.toThrow('This file is over 32 MiB.');
});

it.each([
  ['report.pdf',0,'PDF · 0 B'],['earlier.ZIP',1536,'ZIP · 1.5 KiB'],
  ['dump',2*1024*1024,'File · 2 MiB'],['report.pdf',undefined,'PDF'],
] as const)('shows name suffix and known size for %s', (name,size,detail)=>{
  expect(fileDetails(name,size)).toBe(detail);
});

it.each([
  [200,'0',{size:0}], [200,'4096',{size:4096}], [200,null,{}], [200,'not a size',{}],
  [403,null,{unavailable:'On the computer only'}], [404,null,{unavailable:'No longer there'}], [503,null,{}],
] as const)('uses HEAD metadata without loading the file (status %d; length %s)',async(status,length,want)=>{
  const fetch=vi.fn<typeof globalThis.fetch>().mockResolvedValue(new Response(null,{status,
    headers:length===null?{}:{'content-length':length},
  }));
  await expect(fileHead('/store/1/file +.pdf',fetch)).resolves.toEqual(want);
  expect(fetch).toHaveBeenCalledWith('/file?p=%2Fstore%2F1%2Ffile%20%2B.pdf',expect.objectContaining({method:'HEAD',mode:'same-origin'}));
});

it('does not mark a file gone on a network failure',async()=>{
  const fetch=vi.fn<typeof globalThis.fetch>().mockRejectedValue(new TypeError('offline'));
  await expect(fileHead('/store/1/file.pdf',fetch)).resolves.toEqual({});
});

it.each([
  ['capture.png', 'text/plain; charset=utf-8', 262_144, 'no-store', {size:262_144,text:true}],
  ['report.md', 'text/plain; charset=utf-8', 262_145, 'no-store', {size:262_145}],
  ['report.md', 'application/octet-stream', 14, 'private, max-age=31536000, immutable', {size:14,immutable:true}],
  ['report.md', 'text/plain', 14, 'no-store, immutable', {size:14,text:true}],
  ['report.md', 'text/html', 14, 'no-store', {size:14}],
] as const)('offers a reader only for small plain text from HEAD (%s, %s, %d)',async(name,type,length,cache,want)=>{
  const read=vi.fn<typeof fetch>().mockResolvedValue(new Response(null,{headers:{
    'content-type':type,'content-length':String(length),'cache-control':cache,
  }}));
  await expect(fileHead(`/docs/plans/${name}`,read)).resolves.toEqual(want);
});

const report='/docs/plans/report +.md', plain='text/plain; charset=utf-8';
function textResponse(chunks:Uint8Array[],headers:HeadersInit={'content-type':plain}) {
  const cancelled=vi.fn();let cursor=0;
  const body=new ReadableStream<Uint8Array>({pull(stream){
    const chunk=chunks.at(cursor++);
    if(chunk) stream.enqueue(chunk);else stream.close();
  },cancel:cancelled},{highWaterMark:0});
  return {read:vi.fn<typeof fetch>().mockResolvedValue(new Response(body,{headers})),cancelled};
}

it('decodes UTF-8 split across network chunks as literal report text',async()=>{
  const text='# Résumé\n<em>Field notes</em>\n', bytes=new TextEncoder().encode(text);
  const {read}=textResponse([bytes.slice(0,4),bytes.slice(4,8),bytes.slice(8)]);
  await expect(fileText(report,new AbortController().signal,read)).resolves.toBe(text);
  expect(read).toHaveBeenCalledWith('/file?p=%2Fdocs%2Fplans%2Freport%20%2B.md',
    expect.objectContaining({mode:'same-origin',redirect:'error',signal:expect.any(AbortSignal)}));
});

it('accepts exactly 256 KiB of text even without a length header',async()=>{
  const {read}=textResponse([new Uint8Array(262_144).fill(65)]);
  await expect(fileText(report,new AbortController().signal,read)).resolves.toBe('A'.repeat(262_144));
});

it.each([undefined,'5'])('cancels preview on the byte past the cap (declared length %s)',async(length)=>{
  const headers=new Headers({'content-type':plain});if(length) headers.set('content-length',length);
  const {read,cancelled}=textResponse([new Uint8Array(262_144),new Uint8Array([65])],headers);
  await expect(fileText(report,new AbortController().signal,read)).rejects.toThrow();
  expect(cancelled).toHaveBeenCalledTimes(1);
});

it.each([[404,plain],[200,'text/html'],[200,'application/octet-stream']])('rejects a changed GET after a successful HEAD (%d, %s)',async(status,type)=>{
  const read=vi.fn<typeof fetch>().mockResolvedValue(new Response('different bytes',{status,headers:{'content-type':type}}));
  await expect(fileText(report,new AbortController().signal,read)).rejects.toThrow();
});

it('cancels an outstanding report request when its viewer closes',async()=>{
  const controller=new AbortController();
  const read:typeof fetch=(_url,init)=>new Promise((_ok,fail)=>{
    const signal=init?.signal;
    signal?.addEventListener('abort',()=>fail(signal.reason),{once:true});
  });
  const open=fileText(report,controller.signal,read);
  controller.abort();
  await expect(open).rejects.toMatchObject({name:'AbortError'});
});

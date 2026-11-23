import { record } from './wire.ts';

const MAX_FILE=32*1024*1024, MAX_TEXT=256*1024;
const tooBig='This file is over 32 MiB.';
const sameOrigin={mode:'same-origin',redirect:'error'} as const;

async function shrink(file:File) {
  const image=await createImageBitmap(file);
  const canvas=document.createElement('canvas'), ctx=canvas.getContext('2d');
  try {
    if(!ctx) throw new Error('No canvas');
    const scale=Math.min(1,2048/Math.max(image.width,image.height));
    canvas.width=Math.max(1,Math.round(image.width*scale));canvas.height=Math.max(1,Math.round(image.height*scale));
    ctx.fillStyle='#fff';ctx.fillRect(0,0,canvas.width,canvas.height);
    ctx.drawImage(image,0,0,canvas.width,canvas.height);
  } finally {image.close();}
  const blob=await new Promise<Blob>((ok,fail)=>canvas.toBlob(b=>b?.type==='image/jpeg'?ok(b):fail(), 'image/jpeg',.85));
  const stem=[...file.name.replace(/\.[^.]*$/,'').replace(/^\.+/,'')].slice(0,96).join('')||'photo';
  return {blob,name:stem+'.jpg'};
}

export async function preparePick(file:File,photo:boolean) {
  const pick=photo ? await shrink(file).catch(()=>{
    throw new Error("Can't shrink this photo. Send it from Files to send it as is.");
  }) : {blob:file,name:file.name};
  if(pick.blob.size>MAX_FILE) throw new Error(tooBig);
  return {...pick,photo};
}

export const fileUrl=(path:string)=>'/file?p='+encodeURIComponent(path);
export function fileDetails(name:string,size?:number) {
  const kind=/\.([a-z0-9]{1,12})$/i.exec(name)?.[1]?.toUpperCase()??'File';
  if(size===undefined) return kind;
  const kib=size/1024;
  return kind+' · '+(kib>=1024 ? `${+(kib/1024).toFixed(1)} MiB` : kib>=1 ? `${+kib.toFixed(1)} KiB` : `${size} B`);
}

const plain=(res:Response)=>res.headers.get('content-type')?.split(';',1).at(0)?.trim().toLowerCase()==='text/plain';
function length(res:Response) {
  const len=res.headers.get('content-length');
  return len&&/^\d+$/.test(len)?Number(len):NaN;
}
export async function fileHead(path:string,read=fetch):Promise<{size?:number;text?:true;immutable?:true;unavailable?:string}> {
  const res=await read(fileUrl(path),{...sameOrigin,method:'HEAD',signal:AbortSignal.timeout(15_000)}).catch(()=>undefined);
  if(res?.status===403) return {unavailable:'On the computer only'};
  if(res?.status===404) return {unavailable:'No longer there'};
  if(!res?.ok) return {};
  const size=length(res);
  if(!(size<=MAX_FILE)) return {};
  const cache=res.headers.get('cache-control');
  return {size,...(plain(res)&&size<=MAX_TEXT&&{text:true}),
    ...(cache==='private, max-age=31536000, immutable'&&{immutable:true})};
}

export async function fileText(path:string,signal:AbortSignal,read=fetch):Promise<string> {
  const res=await read(fileUrl(path),{...sameOrigin,signal:AbortSignal.any([signal,AbortSignal.timeout(15_000)])});
  const body=res.body, failed="Couldn't preview this file.";
  if(!res.ok||!plain(res)||length(res)>MAX_TEXT||!body) {
    void body?.cancel().catch(()=>{});
    throw new Error(failed);
  }
  const reader=body.getReader(), decoder=new TextDecoder();let bytes=0, text='';
  try {
    for(;;) {
      const {done,value}=await reader.read();
      if(done) return text+decoder.decode();
      bytes+=value.byteLength;
      if(bytes>MAX_TEXT) {void reader.cancel().catch(()=>{});throw new Error(failed);}
      text+=decoder.decode(value,{stream:true});
    }
  } finally {reader.releaseLock();}
}

export function preview(canvas:HTMLCanvasElement,blob:Blob) {
  void createImageBitmap(blob).then(image=>{
    const scale=64/Math.max(image.width,image.height);
    canvas.width=Math.max(1,Math.round(image.width*scale));canvas.height=Math.max(1,Math.round(image.height*scale));
    canvas.getContext('2d')?.drawImage(image,0,0,canvas.width,canvas.height);image.close();
  }).catch(()=>{});
}

const uploadHints:Record<number,string>={403:'This upload is not allowed.',404:'Files are not set up.',405:'Files are not set up.',
  413:tooBig,503:'Uploads are busy. Try again.',507:'The file store is full.'};
export async function upload(blob:Blob,name:string,signal:AbortSignal) {
  const failed="Couldn't upload this file.";
  const res=await fetch('/upload?name='+encodeURIComponent(name),{...sameOrigin,method:'POST',body:blob,signal})
    .catch(()=>{throw new Error(failed);});
  if(!res.ok) throw new Error(uploadHints[res.status]??failed);
  const info=record(await res.json().catch(()=>null));
  if(typeof info?.path!=='string'||!info.path.startsWith('/')||typeof info.size!=='number') throw new Error(failed);
  return {path:info.path,size:info.size};
}

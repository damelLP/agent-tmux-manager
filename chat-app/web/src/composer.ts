import type { ChannelRow } from './wire.ts';

export function mentionRange(text:string,start:number,end:number) {
  if(start!==end) return;
  const prefix=/(?:^|[\s([{<'"‘“«‹])@([a-z\d-]*(?:\/[a-z\d-]*)?)$/i.exec(text.slice(0,start))?.at(1);
  if(prefix===undefined) return;
  const from=start-prefix.length-1;
  const token=/^[^\s!"#$%&'()*+,.:;<=>?\[\]\\^`{|}~‘’“”«»‹›…–—]*/.exec(text.slice(from+1))?.at(0)??'';
  if(/^[a-z\d-]*(?:\/[a-z\d-]*)?$/i.test(token)) return {start:from,end:from+1+token.length,query:prefix.toLowerCase()};
}

export function mentionChoices(channel:Pick<ChannelRow,'members'|'former'>|undefined,seats:readonly string[],query:string) {
  const members=channel?.members.map(m=>m.address).sort()??[];
  return [...new Set([...members,...[...seats].sort()])].filter(id=>/^[a-z\d][a-z\d-]{0,31}\/[a-z\d][a-z\d-]{0,31}$/.test(id)&&
    !channel?.former.includes(id)&&(id.startsWith(query)||id.split('/').at(1)?.startsWith(query))).slice(0,6);
}

export function sendControl(live:boolean,text:string,attempt?:'sending'|'unknown',hasPhoto=false) {
  const retry=attempt==='unknown';
  const label=retry?'Retry':'Send';
  return {disabled:!live||(!retry&&(!!attempt||!hasPhoto&&!text.trim())),retry,label};
}

type EnterKeys=Pick<KeyboardEvent,'key'|'shiftKey'|'ctrlKey'|'altKey'|'metaKey'|'isComposing'|'keyCode'|'repeat'|'defaultPrevented'>;

// Some IMEs confirm with keyCode 229 after the composing flag has cleared.
export const plainKey=(e:EnterKeys)=>!e.defaultPrevented&&!e.shiftKey&&!e.ctrlKey&&!e.altKey&&!e.metaKey&&!e.isComposing&&e.keyCode!==229;

export function enterAction(e:EnterKeys,touchLike:boolean,freshReady:boolean):'native'|'block'|'send' {
  if(touchLike||e.key!=='Enter'||!plainKey(e)) return 'native';
  return freshReady&&!e.repeat?'send':'block';
}

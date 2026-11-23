// The parser can emit only these fixed tags. A string is prose; `text` is opaque.
export type Markup = string | {tag:'div'|'blockquote'|'ul'|'li'|'strong'|'em';children:Markup[]}
  | {tag:'ol';children:Markup[];start:number} | {tag:'pre';children:Markup[];lines?:number}
  | {tag:'code';text:string} | {tag:'a';text:string;href:string};

function text(out:Markup[], value:string) {
  if(!value) return;
  const last=out.at(-1);
  if(typeof last==='string') {out.pop();value=last+value;}
  out.push(value);
}

function link(value:string) {
  // Require an explicit authority; URL alone repairs missing slashes and strips some controls.
  if(!/^https?:\/\/[^/@?#]+(?:[/?#]|$)/i.test(value)||/[\s\\\x00-\x1f\x7f-\x9f]/.test(value)) return;
  try {return new URL(value).href;} catch { /* invalid destinations are literal */ }
}

function bareTarget(raw:string) {
  const excess=new Map<string,number>();
  let end=raw.length;
  for(;end>0;end--) {
    const tail=raw.charAt(end-1);
    if(/[.,;:!?]/.test(tail)) continue;
    const open='([{'.charAt(')]}'.indexOf(tail));
    if(!open) break;
    // Count each kind of bracket only once, and only if it appears in the suffix.
    const n=excess.get(tail)??raw.split(tail).length-raw.split(open).length;
    if(n<=0) break;
    excess.set(tail,n-1);
  }
  return raw.slice(0,end);
}

const inlineMarks=/(?<!`)`([^`]+)`(?!`)|!?\[([^\[\]]*)\]\(([^()]*)\)|(?<![^\s([<{"'])(https?:\/\/[^\s\x00-\x1f\x7f-\x9f"'<>`*]+)|([*_])\5*/gi;
const word=/[\p{L}\p{N}_\ud800-\udfff]/u;
function inline(source:string):Markup[] {
  const out:Markup[]=[];
  let newline='';
  for(const line of source.split('\n')) {
    text(out,newline);newline='\n';
    const marks=[...line.matchAll(inlineMarks)] as (RegExpExecArray&{close?:number})[], closes=new Map<string,number>();
    // Collect nearest eligible closers so malformed runs do not cause suffix rescans.
    for(const mark of marks.reverse()) {
      const [raw,,,,,delimiter]=mark, at=mark.index;
      if(!delimiter||raw.length>(delimiter==='_'?1:2)) continue;
      const before=line.charAt(at-1), after=line.charAt(at+raw.length);
      if(/\S/.test(after)&&(delimiter!=='_'||!word.test(before))) mark.close=closes.get(raw);
      if(/\S/.test(before)&&(delimiter!=='_'||!word.test(after))) closes.set(raw,at);
    }
    let cursor=0,end=-1,into=out;
    for(const mark of marks.reverse()) {
      const [raw,code,label,destination='',bare]=mark, at=mark.index;
      text(into,line.slice(cursor,at));cursor=at+raw.length;
      if(at===end) {into=out;end=-1;}
      else if(end===-1&&mark.close!==undefined) {
        into=[];out.push({tag:raw==='**'?'strong':'em',children:into});end=mark.close;
      } else if(code!==undefined) into.push({tag:'code',text:code});
      else if(bare||label?.trim()&&!raw.startsWith('!')) {
        const target=bare?bareTarget(bare):destination, href=link(target);
        if(href) {
          into.push({tag:'a',text:label??target,href});
          if(bare) text(into,raw.slice(target.length));
        } else text(into,raw);
      } else text(into,raw);
    }
    text(out,line.slice(cursor));
  }
  return out;
}

export function parseMessage(source:string):Markup[] {
  const blocks:Markup[]=[];
  let run:string[]=[], tag:'ul'|'ol'|'blockquote'|'div'='div', start=1, opening:string|undefined;
  function flush() {
    if(!run.length) return;
    const children:Markup[]=tag==='ol'||tag==='ul'?run.map(s=>({tag:'li',children:inline(s)})):inline(run.join('\n'));
    blocks.push(tag==='ol'?{tag,children,start}:{tag,children});run=[];
  }
  for(const line of source.replace(/\r\n?/g,'\n').split('\n')) {
    const fence=/^```[\w+-]{0,24}[ \t]*$/.test(line);
    if(opening!==undefined) {
      if(fence&&!line.slice(3).trim()) {
        const code=run.join('\n'), lines=run.length;
        blocks.push({tag:'pre',children:[{tag:'code',text:code}],lines:lines>12||code.length>1200?lines:undefined});
        run=[];opening=undefined;
      } else run.push(line);
      continue;
    }
    if(fence) {flush();opening=line;continue;}
    const [prefix='',number]=line.match(/^(?:#{1,3} |> ?|(?:(\d{1,9})\.|[-*])[ \t](?=.*\S))/)??[];
    const value=line.slice(prefix.length);
    if(prefix.startsWith('#')) {flush();blocks.push({tag:'strong',children:inline(value)});continue;}
    const next=number?'ol':prefix.startsWith('>')?'blockquote':prefix?'ul':'div';
    if(tag!==next||!run.length) {flush();tag=next;start=Number(number);}
    run.push(value);
  }
  if(opening===undefined) flush();
  else blocks.push({tag:'div',children:[[opening,...run].join('\n')]});
  return blocks;
}

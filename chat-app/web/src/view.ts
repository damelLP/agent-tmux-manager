import { allowedOptions, canMutate, cards, decisionBusy, decisionFailure, eligiblePicks, incomingOwnerMention, matchesAction, maxPhotos, provisionalPick, roots, sameView, scopeKey, threadHead, timeline } from './state.ts';
import type { ChatState, DecisionAction, OutboxItem, Phase } from './state.ts';
import type { SessionIO, UiInput } from './session.ts';
import { enterAction, mentionChoices, mentionRange, plainKey, sendControl } from './composer.ts';
import { parseMessage } from './markup.ts';
import type { Markup } from './markup.ts';
import { continuesMessage } from './message-group.ts';
import type { GroupRow } from './message-group.ts';
import { fileDetails, fileText, fileUrl, preparePick, preview } from './photos.ts';
import { bindPull } from './pull.ts';
import { bindPostPress } from './post-press.ts';
import { preserveScroll } from './scroll.ts';
import { attachmentName, beforeId, bodyError, reactionEmoji } from './wire.ts';
import type { ChannelRow, DecisionRow, PostId, PostRow, Section } from './wire.ts';

const status:Record<Phase,readonly [string,string?,string?]>={
  live:['Live'],reconnecting:['Reconnecting','Still reconnecting','Showing what you had. Your draft is kept.'],
  offline:['Offline',"You're offline",'Showing what you had. Your draft is kept.'],
  unreachable:["Can't reach","Can't reach your computer",'Is Tailscale on? Is the computer awake?'],
  'daemon-down':['atm stopped',"Your computer is up, but atm isn't running",'Start atm on the computer; this reconnects by itself.'],
  identity:['Not allowed',"This device isn't allowed","It's signed in to Tailscale as someone else."],
  origin:['Origin mismatch','Origin mismatch','Open atm from its Tailscale address.'],
};
const reactionNames=['Thumbs up','Check mark','Eyes','Cross mark'];
const groups:readonly [Section,string][]=[['chief','Chief of staff'],['lead','Leads'],['channel','Channels'],['direct','Direct messages']];
const palette=['#6cc5b6','#e79aa6','#8fb3ff','#c9c08f','#a9c9e8'];
const tint = (id:string) => id.endsWith('/chief') ? '#b7a4ff' : palette.at([...id].reduce((n,c)=>n+c.charCodeAt(0),0)%palette.length) ?? 'var(--paper)';
const initials = (id:string) => id==='owner' ? 'Y' :
  id.split('/').at(-1)?.match(/[a-z0-9]+/gi)?.slice(0,2).map(part=>part.charAt(0).toUpperCase()).join('') || '?';
const person = (id:string,channel?:ChannelRow) => id === 'owner' ? 'You' : id.startsWith(`${channel?.id}/`) ? id.split('/').at(-1) ?? id : id.replace('/',' ');
const label = (channel:ChannelRow) => channel.kind === 'project' ? `#${channel.id}` : channel.members.map(m=>m.address).filter(a=>a!=='owner').map(a=>person(a)).join(' and ') || channel.id;
const clock = new Intl.DateTimeFormat(undefined,{hour:'2-digit',minute:'2-digit',hourCycle:'h23'});
function replyAge(at:string,now:number) {
  const minutes=Math.floor((now-Date.parse(at))/60_000);
  return minutes<1?'just now':minutes<60?`${minutes}m`:minutes<1440?`${Math.floor(minutes/60)}h`:`${Math.floor(minutes/1440)}d`;
}
const el = <K extends keyof HTMLElementTagNameMap>(tag:K,cls='',copy?:string) => {
  const node=document.createElement(tag); node.className=cls; if(copy!==undefined) node.textContent=copy; return node;
};
function get<T extends HTMLElement=HTMLElement>(id:string) {
  const node=document.getElementById(id);
  if (!node) throw new Error(`Missing chat shell element: ${id}`);
  return node as T;
}
const say = (node:Element,copy:string) => {if(node.textContent!==copy) node.textContent=copy;};
function button(copy:string,act:(b:HTMLButtonElement)=>void,cls='') {
  const b=el('button',cls,copy); b.type='button'; b.addEventListener('click',()=>act(b)); return b;
}
const actionLabel=({option,pick}:DecisionAction)=>option===undefined?'Withdraw':pick===undefined?option:
  `${pick===option?'Keep':'Overrule:'} ${option}`;
function list<T>(box:HTMLElement,items:readonly T[],key:(t:T)=>string,make:(t:T)=>HTMLElement,signature:(t:T)=>string) {
  const old=new Map([...box.children].map(e=>[(e as HTMLElement).dataset.key,e as HTMLElement]));
  let next=box.firstElementChild;
  for (const item of items) {
    const id=key(item), sig=signature(item), prior=old.get(id);
    const row=prior?.dataset.signature===sig && prior ? prior : make(item);
    if (row!==prior) {row.dataset.key=id;row.dataset.signature=sig;if(prior===next) next=prior?.nextElementSibling ?? null;prior?.remove();}
    if (row!==next) box.insertBefore(row,next);
    next=row.nextElementSibling;old.delete(id);
  }
  for (const node of old.values()) node.remove();
}
function appendBody(body:HTMLElement,tokens:readonly Markup[],prose:(into:HTMLElement,text:string)=>void) {
  for(const token of tokens) {
    if(typeof token==='string') {prose(body,token);continue;}
    const node=el(token.tag);
    if('text' in token) node.textContent=token.text;
    else appendBody(node,token.children,prose);
    if(token.tag==='ol') node.setAttribute('start',String(token.start));
    if(token.tag==='a') {
      const a=node as HTMLAnchorElement;a.href=token.href;a.target='_blank';a.rel='noopener noreferrer';
    }
    if(token.tag==='pre') {
      node.tabIndex=0;
      if(token.lines) {
        const details=el('details'), more=`Show more · ${token.lines} lines`, summary=el('summary','',more);
        details.ontoggle=()=>say(summary,details.open?'Show less':more);
        details.append(summary,node);body.append(details);continue;
      }
    }
    body.append(node);
  }
}
function empty(box:HTMLElement,copy:string) {
  if(box.firstElementChild?.className!=='empty' || box.children.length!==1) box.replaceChildren(el('p','empty',copy));
  else say(box.firstElementChild,copy);
}

export function mount(app:HTMLElement,emit:(u:UiInput|{type:'back'})=>void,saved:unknown,blob:(id:string)=>Blob|undefined,
  fileHead:SessionIO['fileHead']) {
  const side=get('side-log'), channelLog=get('channel-log'), threadLog=get('thread-log');
  const pull=bindPull(side,channelLog,source=>emit({type:'refresh',source}));
  const nav=get('channels'), needs=get('needs'), needsHome=get('needs-home'), needsWide=get('needs-wide');
  const settingsOpen=get('open-settings'), settingsBack=get('settings-back');
  settingsOpen.querySelector('.avatar')?.append(initials('owner'));
  settingsOpen.addEventListener('click',()=>emit({type:'settings'}));
  const needsTitle=get('needs-title'), channelTitle=get('channel-title'), dialog=get<HTMLDialogElement>('decision-confirm');
  const accept=get('decision-accept'), reasonField=get('decision-reason'), input=get<HTMLInputElement>('decision-input');
  const modalTitle=get('decision-title'), modalCopy=get('decision-copy');
  const viewer=get<HTMLDialogElement>('photo-view'), full=get<HTMLImageElement>('full-photo');
  const report=get('file-text'), markdown=get('file-markdown'), fallback=get<HTMLAnchorElement>('file-download');
  let readingFile:AbortController|undefined;
  function clearViewer() {
    readingFile?.abort();readingFile=undefined;
    full.hidden=report.hidden=markdown.hidden=fallback.hidden=true;
    full.removeAttribute('src');report.textContent='';markdown.replaceChildren();fallback.removeAttribute('href');
  }
  viewer.onclose=()=>{if(!viewer.open)clearViewer();};
  function showFile(name:string) {
    viewer.setAttribute('aria-label',name);
    if(!viewer.open) viewer.showModal();
  }
  function download(link:HTMLAnchorElement,url:string,name:string) {
    link.href=url;link.download=name;return link;
  }
  function attachment(path:string) {
    const name=attachmentName(path)??'file';
    if(!path.startsWith('/')) return el('span','file-chip',name);
    const url=fileUrl(path), chip=()=>{
      const open=button(name,()=>{
        clearViewer();const request=readingFile=new AbortController();
        report.hidden=false;say(report,'Opening…');showFile(name);
        const current=()=>readingFile===request&&viewer.open;
        void fileText(path,request.signal).then(text=>{
          if(!current()) return;
          if(!/\.md$/i.test(name)) {say(report,text);return;}
          appendBody(markdown,parseMessage(text),(node,prose)=>node.append(prose));
          report.hidden=true;markdown.hidden=false;
        },()=>{
          if(!current()) return;
          say(report,"Couldn't preview this file.");download(fallback,url,name).hidden=false;
        });
      },'file-chip'), info=el('small','',fileDetails(name));
      open.disabled=true;open.append(info);
      void fileHead(path).then(({size,text,unavailable})=>{
        say(info,unavailable??(size===undefined?"Couldn't check this file.":fileDetails(name,size)));
        if(text) open.disabled=false;
        else if(!unavailable) {
          const link=download(el('a','file-chip',name),url,name);link.append(info);open.replaceWith(link);
        }
      });
      return open;
    };
    if(!/\.(jpe?g|png|webp|gif)$/i.test(name)) return chip();
    const image=el('img'), thumb=button('',()=>{
      clearViewer();full.src=image.src;full.alt=name;full.hidden=false;showFile(name);
    },'thumbnail');
    thumb.disabled=true;image.onload=()=>{thumb.disabled=false;};
    image.loading='lazy';image.alt=name;image.onerror=()=>thumb.replaceWith(chip());
    image.src=url+'&thumb=1';thumb.append(image);
    return thumb;
  }
  const threadTitle=get('thread-title');
  needsTitle.tabIndex=channelTitle.tabIndex=threadTitle.tabIndex=-1;
  const cardBox=get('cards'), postBox=get('posts'), replies=get('replies'), older=get<HTMLButtonElement>('older');
  const recovery=el('details'), recoveryTitle=el('summary','sect'), recoveryRows=el('div');
  recovery.append(recoveryTitle,recoveryRows);cardBox.after(recovery);
  const back=get<HTMLButtonElement>('thread-back'), bars=[...app.querySelectorAll('.conn')];
  const connLabel=get('connection-label'), banner=get('connection-banner');
  const bannerHead=banner.querySelector('strong'), bannerSub=banner.querySelector('small');
  const bookmarks=Array.isArray(saved)?saved:[];
  const sidePos=preserveScroll(side,false,bookmarks.at(0)), channelPos=preserveScroll(channelLog,true,bookmarks.at(1));
  const threadPos=preserveScroll(threadLog,true,bookmarks.at(2)), needsPos=preserveScroll(needs,false,bookmarks.at(3));
  const wide=matchMedia('(min-width: 2200px)'), desktop=matchMedia('(min-width: 1100px)');
  let s:ChatState, focused:number|undefined, channel:ChannelRow|undefined, reading=0, busy=false;
  const findPost=(id:string,thread:boolean)=>(thread?replies:postBox).querySelector<HTMLElement>(`[data-post-id="${id}"]`);
  type ConfirmOrigin=HTMLButtonElement|(()=>void);
  let entry:number|undefined, confirmation:{from:ConfirmOrigin;action?:DecisionAction}|undefined;
  function dismiss() {
    const from=confirmation?.from;if(!from) return;confirmation=undefined;
    dialog.close();input.value=modalTitle.textContent=modalCopy.textContent='';accept.onclick=null;
    if(typeof from==='function') {from();return;}
    const root=from.closest<HTMLElement>('.msg');
    (from.isConnected&&!from.disabled?from:root?.isConnected?root:needsTitle.getClientRects().length?needsTitle:channelTitle)
      .focus({preventScroll:true});
  }
  const bulk=get<HTMLButtonElement>('keep-picks'), bulkStatus=get('keep-status'), cancel=get('decision-cancel');cancel.onclick=dismiss;
  const keepLabel=(n:number)=>`Keep all agents' picks (${n})`;
  bulk.onclick=()=>{
    const picks=eligiblePicks(s), n=picks.length;
    if(n>=2) confirm(bulk,`${keepLabel(n)}?`,`This confirms ${n} provisional picks. Pending decisions are not changed.`,
      `Keep all ${n} picks`,()=>emit({type:'keep-all',picks}));
  };
  dialog.oncancel=e=>{e.preventDefault();dismiss();};
  function confirm(from:ConfirmOrigin,title:string,copy:string,yes:string,commit:(reason?:string)=>void,
    action?:DecisionAction,retry=false) {
    confirmation={from,action};
    say(modalTitle,title);say(modalCopy,copy);say(accept,yes);
    reasonField.hidden=!action||action.option!==undefined;input.value=action?.reason??'';
    input.readOnly=retry;
    accept.onclick=()=>{
      const reason=input.value.trim()||undefined;
      input.setCustomValidity(reason&&new TextEncoder().encode(reason).length>8192?'Reason is over 8 KiB.':'');
      if(!input.reportValidity()) return;
      commit(reason);dismiss();
    };
    dialog.showModal();
  }
  const sheet=el('dialog','message-sheet'), panel=el('div','sheet-panel'), clipboardStatus=el('small','ask-state');
  sheet.id='message-actions';sheet.setAttribute('aria-label','Message actions');clipboardStatus.setAttribute('role','status');
  sheet.append(panel);app.append(sheet);
  const rowOpeners=new WeakMap<HTMLElement,()=>boolean>();
  const press=bindPostPress(app,row=>rowOpeners.get(row)?.()??false);
  let opened:{row:HTMLElement;trigger:HTMLButtonElement;restore:()=>void}|undefined, outside=false;
  function closeActions(restore=true) {
    const target=opened;if(!target) return;
    opened=undefined;sheet.close();panel.replaceChildren();target.trigger.setAttribute('aria-expanded','false');
    if(restore) target.restore();
  }
  sheet.oncancel=e=>{e.preventDefault();closeActions();};
  sheet.onpointerdown=e=>{outside=e.target===sheet;};
  sheet.onclick=e=>{if(outside&&e.target===sheet) closeActions();};
  sheet.onkeydown=e=>{
    if(e.repeat&&e.key==='Enter') {e.preventDefault();return;}
    if(e.key!=='Tab') return;
    const buttons=sheet.querySelectorAll<HTMLButtonElement>('button:not(:disabled)');
    const first=buttons.item(0),last=buttons.item(buttons.length-1);
    if(document.activeElement===(e.shiftKey?first:last)) {e.preventDefault();(e.shiftKey?last:first)?.focus();}
  };
  function actionTrigger(row:HTMLElement,body:string,find:()=>HTMLElement|null,controls:(restore:()=>void)=>HTMLElement[]) {
    const trigger=button('Message actions',b=>show(b),'message-trigger');
    trigger.setAttribute('aria-haspopup','dialog');trigger.setAttribute('aria-expanded','false');
    trigger.setAttribute('aria-controls',sheet.id);
    function show(from:HTMLElement) {
      if(sheet.open||dialog.open||viewer.open||!row.isConnected||!row.getClientRects().length) return false;
      const restore=()=>{
        const current=find(), target=from===trigger?current?.querySelector<HTMLElement>('.message-trigger'):current;
        const heading=s.pane==='thread'?threadTitle:s.pane==='home'?needsTitle:channelTitle;
        (target?.getClientRects().length?target:heading).focus({preventScroll:true});
      };
      const invocation=opened={row,trigger,restore};
      let copying=false;
      trigger.setAttribute('aria-expanded','true');outside=false;say(clipboardStatus,'');
      panel.replaceChildren(...controls(restore),button('Copy text',async()=>{
        if(copying||opened!==invocation) return;
        copying=true;say(clipboardStatus,'');
        try {await navigator.clipboard.writeText(body);}
        catch {copying=false;if(opened===invocation) say(clipboardStatus,"Couldn't copy text.");return;}
        if(opened===invocation) closeActions();
      }),clipboardStatus,button('Close',()=>closeActions()));
      sheet.showModal();return true;
    }
    row.onkeydown=e=>{
      if(e.target===trigger&&e.repeat&&e.key==='Enter') {e.preventDefault();return;}
      if(e.shiftKey&&e.key==='F10'&&(e.target===row||e.target===trigger)&&show(e.target===trigger?trigger:row)) e.preventDefault();
    };
    rowOpeners.set(row,()=>show(row));return trigger;
  }
  function moveNeeds() {
    const parent=wide.matches?needsWide:needsHome;
    if (needs.parentElement!==parent) {
      const active=document.activeElement;
      parent.append(needs);
      if (active instanceof HTMLElement && needs.contains(active)) active.focus({preventScroll:true});
    }
  }
  function backLabel(){back.textContent=desktop.matches?'×':'‹';back.setAttribute('aria-label',desktop.matches?'Close thread':'Back');}
  wide.addEventListener('change',moveNeeds); desktop.addEventListener('change',backLabel); moveNeeds(); backLabel();
  for(const b of [get('home-back'),back,settingsBack]) b.addEventListener('click',()=>emit({type:'back'}));
  older.addEventListener('click',()=>emit({type:'older'}));
  const logs=[channelLog,threadLog], divider=el('div','new-line','New');
  const port=()=>s.pane==='thread'?threadLog:channelLog;
  const jumpNew=button('Jump to new',()=>{
    divider.scrollIntoView({block:'start'});
    (divider.nextElementSibling as HTMLElement|null)?.focus({preventScroll:true});
  },'older jump-new');
  let refont=false, reachedNew=false;
  function readSoon() {
    if(reading) return;
    reading=requestAnimationFrame(()=>{
      reading=0;
      channelForm.fit(refont);threadForm.fit(refont);refont=false;
      if(divider.isConnected&&!reachedNew&&document.visibilityState==='visible') {
        const line=divider.getBoundingClientRect(), box=port().getBoundingClientRect();
        reachedNew=!!line.height&&line.top<box.bottom&&line.bottom>box.top;
      }
      jumpNew.hidden=!divider.isConnected||reachedNew;
      if (s.phase!=='live'||!s.channel||document.visibilityState!=='visible') return;
      let through:PostId|undefined;
      for(const log of logs) {
        if(!log.clientHeight) continue;
        const box=log.getBoundingClientRect();
        for(const end of log.querySelectorAll<HTMLElement>('[data-end]')) {
          const pos=end.getBoundingClientRect(), id=end.dataset.end as PostId|undefined;
          if(id&&pos.height&&pos.top>=box.top&&pos.bottom<=box.bottom&&(!through||beforeId(through,id))) through=id;
        }
      }
      if(through) emit({type:'visible',channel:s.channel,through});
    });
  }
  const reader=new ResizeObserver(()=>{
    if(opened&&!opened.row.getClientRects().length) closeActions();
    readSoon();
  });
  for(const log of logs) log.addEventListener('scroll',readSoon,{passive:true});
  for(const box of [...logs,postBox,replies]) reader.observe(box);
  const coarse=matchMedia('(any-pointer: coarse)');
  const composer = (id:string) => {
    const form=get<HTMLFormElement>(`${id}-form`), readOnly=get(`${id}-readonly`);
    const input=form.querySelector('textarea'), send=form.querySelector<HTMLButtonElement>('button[type=submit]'), hint=form.querySelector('p');
    const placeholder=el('span','compose-placeholder');placeholder.setAttribute('aria-hidden','true');input?.after(placeholder);
    const share=form.querySelector<HTMLInputElement>('input[type=checkbox]'), strip=form.querySelector<HTMLElement>('.picks');
    const attach=form.querySelector<HTMLButtonElement>('.attach-plus'), attachChoices=form.querySelector<HTMLElement>('.attach-choices');
    let key='', measured='', width=0, preparing=false, controls=()=>{};
    const mentions=el('ul','mention-picks');mentions.id=`${id}-mentions`;mentions.hidden=true;
    mentions.setAttribute('role','listbox');mentions.setAttribute('aria-label','Mention a seat');form.prepend(mentions);
    input?.setAttribute('aria-autocomplete','list');input?.setAttribute('aria-controls',mentions.id);
    let armed=false, selected=0, choices:string[]=[];
    const closeMentions=()=>{armed=false;mentions.hidden=true;input?.removeAttribute('aria-activedescendant');};
    const optionId=(seat:string)=>`${mentions.id}-${seat}`;
    const range=()=>input&&!input.readOnly&&input===document.activeElement&&armed?mentionRange(input.value,input.selectionStart,input.selectionEnd):undefined;
    function showMentions() {
      const span=range();choices=span?mentionChoices(channel,s.seatIds??[],span.query):[];
      mentions.hidden=!choices.length;selected=Math.max(0,Math.min(selected,choices.length-1));
      list(mentions,choices,seat=>seat,seat=>{
        const option=el('li','',`@${seat}`);option.id=optionId(seat);option.setAttribute('role','option');option.onclick=()=>pickMention(seat);return option;
      },seat=>seat);
      const choice=choices.at(selected);
      if(choice) input?.setAttribute('aria-activedescendant',optionId(choice));
      else input?.removeAttribute('aria-activedescendant');
      for(const [i,node] of Array.from(mentions.children).entries()) node.setAttribute('aria-selected',String(i===selected));
    }
    function pickMention(seat:string) {
      const span=range();if(!span||!input||!mentionChoices(channel,s.seatIds??[],span.query).includes(seat)) return;
      closeMentions();input.setRangeText(`@${seat}${span.end===input.value.length?' ':''}`,span.start,span.end,'end');edit();
    }
    mentions.onpointerdown=e=>e.preventDefault(); // Keep the textarea focused for mouse and touch picks.
    input?.addEventListener('blur',closeMentions);input?.addEventListener('selectionchange',showMentions);
    const showChoices=(open:boolean)=>{
      if(attachChoices) attachChoices.hidden=!open;
      attach?.setAttribute('aria-expanded',String(open));
    };
    if(attach&&attachChoices) {
      attach.onclick=()=>showChoices(attachChoices.hidden);
      attach.parentElement?.addEventListener('keydown',e=>{
        if(e.key==='Escape'&&!attachChoices.hidden) {e.preventDefault();showChoices(false);attach.focus();}
      });
    }
    const options=attachChoices?.querySelectorAll('button');
    for(const [i,picker] of form.querySelectorAll<HTMLInputElement>('input[type=file]').entries()) {
      let chosenFor='';
      const option=options?.item(i), photo=picker.accept==='image/*';
      if(option) option.onclick=()=>{chosenFor=key;showChoices(false);attach?.focus();picker.click();};
      picker.addEventListener('change',async()=>{
        const target=chosenFor, selected=Array.from(picker.files??[]);
        picker.value='';chosenFor='';if(!target||key!==target||preparing||!selected.length)return;
        const files=selected.slice(0,maxPhotos-(s.photos.get(target)?.length??0));
        preparing=true;controls();let problem='';
        for(const file of files) {
          try {emit({type:'pick',key:target,...await preparePick(file,photo)});}
          catch(error) {if(error instanceof Error) problem=error.message;}
        }
        preparing=false;controls();
        if(problem||selected.length>files.length) emit({type:'photo-error',key:target,message:problem||'8 files at most'});
      });
    }
    function fit(force=false) {
      if(!input) return;
      const next=input.getBoundingClientRect().width, text=input.value;
      if(!next) {width=0;return;}
      if(!force&&next===width&&text===measured) return;
      width=next;measured=text;
      const top=input.scrollTop;
      input.style.blockSize='auto';
      if(text) input.style.blockSize=`${input.scrollHeight}px`;
      input.scrollTop=top;
    }
    function edit(checked?:boolean){if(input&&key) emit({type:'draft',key,text:input.value,start:input.selectionStart,end:input.selectionEnd,share:checked});}
    input?.addEventListener('input',()=>{armed=true;selected=0;edit();});input?.addEventListener('select',()=>edit());
    share?.addEventListener('change',()=>edit(share.checked));
    input?.addEventListener('keydown',e=>{
      if(!send) return;
      if(!mentions.hidden&&plainKey(e)&&['ArrowUp','ArrowDown','Enter','Escape'].includes(e.key)) {
        e.preventDefault();
        if(e.key==='Escape') closeMentions();
        else if(e.key==='Enter') {const seat=choices.at(selected);if(seat&&!e.repeat) pickMention(seat);}
        else {
          selected=(selected+choices.length+(e.key==='ArrowUp'?-1:1))%choices.length;showMentions();
          mentions.children.item(selected)?.scrollIntoView({block:'nearest'});
        }
        return;
      }
      const action=enterAction(e,navigator.maxTouchPoints>0||coarse.matches,!send.disabled&&!input.readOnly);
      if(action!=='native') e.preventDefault();
      if(action==='send') form.requestSubmit(send);
    });
    form.addEventListener('submit',e=>{
      e.preventDefault();
      if(!key||!send||send.disabled) return;
      closeMentions();if(document.activeElement===send) input?.focus({preventScroll:true});
      emit({type:'submit',key,retry:s.attempts.get(key)?.phase==='unknown'});
    });
    return {fit,closeMentions,paint(next:string,copy:string,present:boolean,writable:boolean){
      if (!input||!send||!hint) return;
      const draft=s.drafts.get(next), attempt=s.attempts.get(next), changed=key!==next;key=next;
      form.hidden=!present||!writable;readOnly.hidden=!present||writable;
      say(placeholder,copy); input.placeholder=copy; input.setAttribute('aria-label',copy); input.readOnly=!!attempt;
      const text=draft?.text??'', checked=attempt?attempt.params.also_in_chat===true:draft?.share===true;
      if(input.value!==text||changed) {input.value=text;input.setSelectionRange(draft?.start??0,draft?.end??0);}
      if(share) {
        share.checked=checked;share.parentElement?.toggleAttribute('hidden',!s.canShare&&!checked);share.disabled=!!attempt;
      }
      const live=present&&writable&&s.phase==='live', photos=s.photos.get(next)??[];
      const blocked=checked&&!s.canShare&&(!!attempt||photos.length>0);
      if(!live||attempt||changed) closeMentions();else showMentions();
      const queueable=present&&writable&&!photos.length&&!attempt&&s.phase!=='identity'&&s.phase!=='origin';
      const control=sendControl(queueable||live&&!blocked,text,attempt?.phase,photos.length>0);
      if(changed) showChoices(false);
      if(strip) {
        strip.hidden=!photos.length;
        list(strip,photos,p=>p.id,p=>{
          const tile=p.photo?el('div'):el('span','file-chip',p.name);
          if(p.photo) {
            const canvas=el('canvas'), bytes=blob(p.id);
            if(bytes) preview(canvas,bytes);
            tile.append(canvas);
          } else tile.append(el('small','',fileDetails(p.name,p.size)));
          const remove=button('×',()=>emit({type:'remove-photo',key:next,id:p.id}));
          remove.setAttribute('aria-label',`Remove ${p.name}`);tile.append(remove);return tile;
        },p=>p.id);
      }
      controls=()=>{
        send.disabled=preparing||control.disabled;
        if(attach) {
          attach.disabled=preparing||!live||!!attempt||photos.length>=maxPhotos;
          if(attach.disabled) showChoices(false);
        }
        for(const b of strip?.querySelectorAll('button')??[]) b.disabled=preparing||!!attempt;
      };
      controls();send.toggleAttribute('data-retry',control.retry);
      if(send.getAttribute('aria-label')!==control.label) send.setAttribute('aria-label',control.label);
      const feedback=s.wakeSummary?.key===next?s.wakeSummary.text:'';
      say(hint,attempt?.phase==='sending'?'Sending…':attempt?'Not sure it sent.'+(blocked?' Update the daemon to retry.':''):s.notices.get(next)??feedback);
      fit();
    }};
  };
  const channelForm=composer('channel'), threadForm=composer('thread');
  const activityLines=[channelLog,threadLog].map(log=>{
    const node=el('div','activity');node.setAttribute('role','status');log.after(node);return node;
  });
  // The logs and their sibling editors get the same pane width changes.
  function fitFonts(){refont=true;readSoon();}
  document.fonts.addEventListener('loadingdone',fitFonts);
  const conversation = (q:DecisionRow) => q.post_id && s.channels.find(c=>c.id===q.channel);
  const question=(r:PostRow)=>!r.thread?s.decisions.find(d=>d.channel===r.channel&&d.post_id===r.id):undefined;
  const actionSig=(q:DecisionRow|undefined)=>q&&[q,s.phase,busy,s.answers.get(q.id),s.notices.get(q.id)];
  function decisionButton(a:DecisionAction,disabled:boolean,retry=false) {
    const b=button(retry?'Retry':actionLabel(a),from=>{
      const send=(action:DecisionAction)=>emit({type:'decision',action});
      if(a.option===undefined) confirm(from,'Withdraw this decision?',
        'This closes the request without choosing an option.','Confirm withdrawal',reason=>send({id:a.id,reason}),a,retry);
      else if(a.pick!==undefined&&a.pick!==a.option) confirm(from,`Overrule with ${a.option}?`,
        `The work done on ${a.pick} is reopened and its owner is told.`,'Confirm overrule',()=>send(a),a);
      else send(a);
    });
    b.disabled=disabled;return b;
  }
  function actions(q:DecisionRow,open:boolean) {
    const out=el('div','opts'), pending=s.answers.get(q.id), disabled=s.phase!=='live'||busy;
    const id=q.id, pick=provisionalPick(q);
    for(const option of allowedOptions(q)) out.append(decisionButton({id,option,pick},disabled||!!pending));
    if(q.status==='pending') out.append(decisionButton({id},disabled||!!pending));
    if(open) out.append(button('Open conversation',()=>emit({type:'open',id}),'open'));
    const msg=pending ? (pending.phase==='unknown'?decisionFailure(pending.action):'Checking answer…') : s.notices.get(id);
    if(msg) out.append(el('small','ask-state',msg));
    if(pending?.phase==='unknown' && matchesAction(q,pending.action)) out.append(
      el('small','',actionLabel(pending.action)),decisionButton(pending.action,disabled,true));
    return out;
  }
  function card(q:DecisionRow) {
    const c=conversation(q), requester=q.request.requester;
    const article=el('article','card'); article.id=article.dataset.scrollId=q.id;article.tabIndex=-1;
    const from=el('strong','author',person(requester,c)); if(requester!=='owner') from.style.color=tint(requester);
    article.append(from,el('small','where',c?.kind==='direct'&&c.writable?'in your DM':c?`in ${label(c)}`:''));
    article.append(el('p','question',q.request.title+(q.request.context?`\n${q.request.context}`:'')));
    if(q.status==='taken_provisionally') article.append(el('small','where','Taken provisionally'));
    article.append(actions(q,!!c)); return article;
  }
  let editing:{id:PostId;thread:boolean;form:HTMLFormElement;field:HTMLTextAreaElement;save:HTMLButtonElement}|undefined;
  const editor=(r:PostRow,thread:boolean)=>editing?.id===r.id&&editing.thread===thread ? editing : undefined;
  function editPost(r:PostRow,thread:boolean) {
    const form=el('form','post-tools edit-post'), field=el('textarea'), save=el('button','','Save');
    field.value=r.body;field.setAttribute('aria-label','Edit message');field.oninput=()=>field.setCustomValidity('');
    const close=()=>{editing=undefined;render(s,entry);};
    form.onsubmit=e=>{
      e.preventDefault();if(save.disabled) return;
      const body=field.value, problem=bodyError(body,r.attachments.length>0);
      field.setCustomValidity(problem==='blank'?'Write a message first.':problem?'Message is over 16 KiB.':'');
      if(field.reportValidity()) emit({type:'post-change',method:'channel.edit',params:{post:r.id,body},onSent:()=>{
        if(editing?.form===form) close();
      }});
    };
    form.append(field,save,button('Cancel',close));editing={id:r.id,thread,form,field,save};render(s,entry);field.focus();
  }
  const postTools=(r:PostRow)=>[canMutate(s,r,'channel.edit'),canMutate(s,r,'channel.remove'),canMutate(s,r,'channel.react'),s.mutating?.has(r.id),s.notices.get(r.id)] as const;
  function reactionButton(row:PostRow,emoji:(typeof reactionEmoji)[number],i:number,close=false) {
    const r=row.reactions?.find(r=>r.emoji===emoji), name=reactionNames.at(i)??emoji;
    const chip=button(r?.count?`${emoji} ${r.count}`:emoji,()=>{
      if(close) closeActions();
      emit({type:'post-change',method:'channel.react',params:{post:row.id,emoji}});
    },'reaction');
    chip.dataset.emoji=emoji;chip.setAttribute('aria-label',r?.count?`${name}, ${r.count}`:name);
    chip.setAttribute('aria-pressed',String(!!r?.mine));chip.disabled=!!s.mutating?.has(row.id);
    return chip;
  }
  function avatar(author:string) {
    const face=el('span','avatar',initials(author));face.setAttribute('aria-hidden','true');
    if(author!=='owner') face.style.backgroundColor=tint(author);
    return face;
  }
  function post(row:PostRow,thread:boolean,quote?:string) {
    const article=el('article','msg');article.dataset.postId=article.dataset.scrollId=row.id;
    if(incomingOwnerMention(row)) article.classList.add('msg--mentioned');
    article.tabIndex=-1;
    const meta=el('div','meta'), author=el('strong','author',person(row.author,channel));
    if(row.session_id) {author.title=`${row.author} · ${row.session_id}`;author.tabIndex=0;author.setAttribute('aria-label',author.title);}
    const time=el('time','',clock.format(new Date(row.posted_at)));time.dateTime=row.posted_at;meta.append(avatar(row.author),author,time);
    if(row.kind==='question') meta.append(el('span','tag','Question'));
    const body=el('div',row.removed?'body post-status':'body');
    const tags=new Set(row.mentions.flatMap(m=>m==='owner'?['@owner','@you']:'seat' in m?[`@${m.seat}`,`@${person(m.seat,channel)}`]:[`@${m.group.project}/${m.group.name}`,`@${m.group.name}`]));
    const former=new Set(channel?.former.flatMap(a=>[`@${a}`,`@${person(a,channel)}`]));
    appendBody(body,parseMessage(row.body),(into,prose)=>{
      for(const text of prose.split(/(@[\w-]+(?:\/[\w-]+)?)/g)) {
        into.append(former.has(text)?el('span','mention',`${text} (closed)`):tags.has(text)?el('span','mention',text):document.createTextNode(text));
      }
    });
    article.append(meta);
    const root=row.thread;
    if(!thread&&root&&row.shared) {
      const label='Replied in a thread';
      article.append(quote?button(`${label} · ${quote}`,()=>emit({type:'thread',id:root}),'reply-count quote'):el('small','quote',label));
    } else if(quote) article.append(el('blockquote','quote',quote));
    article.append(body);
    if(row.attachments.length) {const files=el('div','files');files.append(...row.attachments.map(attachment));article.append(files);}
    if(row.edited&&!row.removed) article.append(el('small','post-status','edited'));
    const end=el('span','sentinel');end.dataset.end=row.id;article.append(end);
    if(!thread&&!row.thread) {
      const q=question(row);
      if(q) article.append(actions(q,false));
      if(row.replies) {
        const reply=button(`${row.replies} ${row.replies===1?'reply':'replies'}`,()=>emit({type:'thread',id:row.id}),'reply-count');
        if(row.last_reply_at) {
          const last=el('time');last.dateTime=row.last_reply_at;last.title=new Date(last.dateTime).toLocaleString();reply.append(last);
        }
        article.append(reply);
      }
    } else if(!row.thread) article.classList.add('thread-root');
    const [canEdit,,canReact,pending,notice]=postTools(row), edit=!row.removed&&editor(row,thread);
    const tools=el('div','post-tools');
    if(edit) {body.hidden=true;edit.save.disabled=!canEdit||!!pending;body.after(edit.form);}
    if(!row.removed) for(const [i,emoji] of reactionEmoji.entries()) {
      const r=row.reactions?.find(r=>r.emoji===emoji);
      if(r?.count) tools.append(canReact?reactionButton(row,emoji,i):el('span','post-status',`${emoji} ${r.count}`));
    }
    if(notice) tools.append(el('small','ask-state',notice));
    if(tools.childElementCount) article.append(tools);
    article.append(actionTrigger(article,row.body,()=>findPost(row.id,thread),restore=>{
      const [edit,remove,react,pending]=postTools(row), controls:HTMLElement[]=[];
      if(react) {
        const group=el('div','post-tools');group.setAttribute('role','group');group.setAttribute('aria-label','Choose reaction');
        group.append(...reactionEmoji.map((emoji,i)=>reactionButton(row,emoji,i,true)));controls.push(group);
      }
      if(!thread&&!row.thread&&channel?.writable) controls.push(button('Reply in thread',()=>{
        closeActions(false);emit({type:'thread',id:row.id});threadTitle.focus({preventScroll:true});
      }));
      if(!editor(row,thread)) {
        if(edit) {
          const action=button('Edit',()=>{closeActions(false);editPost(row,thread);});action.disabled=!!pending;controls.push(action);
        }
        if(remove) {
          const action=button('Remove',()=>{closeActions(false);confirm(restore,'Remove this message?',
            'A removed line stays in the conversation.','Remove message',()=>emit({type:'post-change',method:'channel.remove',params:{post:row.id}}));});
          action.disabled=!!pending;controls.push(action);
        }
      }
      return controls;
    }));
    return article;
  }
  const localKey=(item:OutboxItem)=>`queued:${item.params.client_key}`;
  const scrollRow=(box:HTMLElement,key:string)=>box.querySelector<HTMLElement>(`[data-scroll-id="${CSS.escape(key)}"]`);
  function pendingPost(box:HTMLElement,item:OutboxItem,statusOnly=false) {
    const node=el('article',item.done?'msg outbox sent':'msg outbox'), key=localKey(item);
    node.tabIndex=-1;node.dataset.scrollId=key;
    const meta=el('div','meta');meta.append(avatar('owner'));node.append(meta);
    if(!statusOnly) {meta.append(el('strong','author','You'));node.append(el('div','body',item.params.body));}
    if(box===recoveryRows) {
      const target=s.channels.find(c=>c.id===item.channel), name=target?label(target):item.channel.includes('+')?item.channel:`#${item.channel}`;
      meta.append(el('small','where',item.params.reply_to?`Reply in ${name}`:name));
    }
    if(!item.done) {
      const status=el('small','post-status',item.failed?'Not sent':'Sending…');status.setAttribute('role','status');node.append(status);
    }
    node.append(actionTrigger(node,item.params.body,()=>scrollRow(box,key),()=>{
      if(!item.failed) return [];
      const action=(name:string,type:'outbox-retry'|'outbox-delete')=>button(name,()=>emit({type,id:item.params.client_key,
        onFailure:()=>say(clipboardStatus,"Couldn't change the outbox in this tab."),
      }));
      return [...(box===recoveryRows?[]:[action('Retry','outbox-retry')]),action('Delete','outbox-delete')];
    }));
    return node;
  }
  function postList(box:HTMLElement,rows:readonly PostRow[],inThread:boolean,quote?:(r:PostRow)=>string|undefined) {
    const local=s.outbox.filter(o=>o.channel===channel?.id&&(inThread?o.params.reply_to===s.thread:
      !o.params.reply_to||o.params.also_in_chat||channel?.kind==='direct'));
    // History has no key. Keep it visible and show just the local status until exact proof.
    const statusOnly=(o:OutboxItem)=>o.floor!==undefined&&!o.id&&!o.failed&&rows.some(r=>r.native&&r.author==='owner'&&
      o.params.reply_to===r.thread&&beforeId(`m${BigInt(o.floor??0)}`,r.id));
    const same=new Map(local.filter(o=>o.id).map(o=>[o.id,localKey(o)]));
    const pending=local.filter(o=>!rows.some(r=>r.id===o.id));
    const items=[...rows,...pending];if(!items.length) return 0;
    const prior=new Map([...box.querySelectorAll<HTMLElement>('[data-post-id]')].map(n=>[n.dataset.postId,n.dataset.key]));
    const displayId=(r:PostRow|OutboxItem)=>'params' in r?localKey(r):same.get(r.id)??prior.get(r.id)??r.id;
    const active=document.activeElement;
    const control=active instanceof HTMLButtonElement&&box.contains(active)&&active.matches('.message-trigger,.reaction')?active:undefined;
    const focus=control?.closest<HTMLElement>('.msg')?.dataset;
    list(box,items,displayId,r=>{
      const row='params' in r?pendingPost(box,r,statusOnly(r)):post(r,inThread,quote?.(r));row.dataset.scrollId=displayId(r);return row;
    },r=>JSON.stringify('params' in r?[r,statusOnly(r)]:[r,channel?.former,postTools(r),!!editor(r,inThread),
      !inThread&&[quote?.(r),channel?.writable,actionSig(question(r))]]));
    if(focus&&!control?.isConnected) {
      const row=focus.postId?findPost(focus.postId,inThread):scrollRow(box,focus.scrollId??'');
      const next=[...row?.querySelectorAll<HTMLButtonElement>('.reaction')??[]]
        .find(b=>!b.disabled&&b.dataset.emoji===control?.dataset.emoji);
      (next??row?.querySelector<HTMLElement>('.message-trigger')??row)?.focus({preventScroll:true});
    }
    // Post-only boxes now contain exactly the keyed rows in display order.
    // Patch headers so a new neighbor never replaces a control in a surviving row.
    const target=s.pane===(inThread?'thread':'channel')?s.opening?.target:undefined, now=Date.now();
    let previous:GroupRow|undefined, atNew:HTMLElement|undefined;
    for(const [i,row] of rows.entries()) {
      const node=box.children.item(i) as HTMLElement, meta=node.firstElementChild as HTMLElement;
      if(!inThread&&!row.thread&&row.replies&&row.last_reply_at) {
        const last=node.querySelector('.reply-count time');
        if(last) say(last,` · last reply ${replyAge(row.last_reply_at,now)}`);
      }
      const date=new Date(row.posted_at), author=row.author;
      const boundary=!!(row.decision||row.kind==='question'||(inThread?!row.thread:row.thread||row.replies||question(row)));
      if(row.id===target) { atNew=node;previous=undefined; }
      const next={author,at:+date,day:date.toDateString(),boundary}, hide=continuesMessage(previous,next);
      previous=next;
      if(meta.hidden===hide) continue;
      const hadFocus=hide&&meta.contains(document.activeElement);
      meta.hidden=hide;
      if(hide) {
        node.title=[author==='owner'?'You':author,date.toLocaleString(),row.session_id].filter(Boolean).join(' · ');
        node.setAttribute('aria-label',node.title);
      } else {node.title='';node.removeAttribute('aria-label');}
      if(hadFocus) node.focus({preventScroll:true});
    }
    atNew?.before(divider);
    return items.length;
  }
  function channelItems(holder:Element,rows:ChannelRow[]) {
    list(holder as HTMLElement,rows,c=>c.id,c=>{
      const b=button('',()=>emit({type:'select',channel:c.id}),'item');b.dataset.channel=b.dataset.scrollId=c.id;
      if(c.id===s.channel) b.setAttribute('aria-current','true');
      const marker=el('span','marker',c.kind==='project'?'#':'');marker.style.color=tint(c.members.find(m=>m.address!=='owner')?.address??c.id);b.append(marker,el('span','item-name',label(c)));
      if(c.unread) {b.classList.add('unread');b.append(el('span','badge',String(c.unread)));}
      else if(!c.writable) b.append(el('small','ro','read only'));
      return b;
    },c=>JSON.stringify([c,c.id===s.channel]));
  }
  function render(state:ChatState,nextEntry?:number) {
    const previousPane=s?.pane, departed=s&&(!sameView(s,state)||entry!==nextEntry);
    if(departed||state.phase==='identity'||!state.postMethods?.includes('channel.edit')) editing=undefined;
    if(departed||state.phase==='identity') {press.cancel();closeActions(false);}
    const field=editing?.field===document.activeElement?editing?.field:undefined;
    for(const pos of [sidePos,needsPos,channelPos,threadPos]) {
      if(state.phase==='identity') pos.clear(); else pos.beforePatch();
    }
    divider.remove();
    if(s?.opening!==state.opening) {reachedNew=false;jumpNew.hidden=true;}
    if(departed) {pull.cancel();app.dataset.refreshing='';channelForm.closeMentions();threadForm.closeMentions();}
    if(state.refreshing!==s?.refreshing) app.dataset.refreshing=state.refreshing??'';
    entry=nextEntry;s=state; busy=decisionBusy(s); channel=s.channels.find(r=>r.id===s.channel);
    app.dataset.phase=s.phase;app.dataset.pane=s.pane;app.dataset.thread=String(!!s.thread);
    settingsOpen.hidden=s.pane!=='home'||s.phase==='identity';
    const [dot,title,sub]=status[s.phase];bars.forEach(b=>say(b,dot));say(connLabel,app.dataset.refreshing?'Refreshing…':dot);
    banner.dataset.phase=s.phase;banner.hidden=!s.outboxPurgeFailed&&(!title||(s.phase==='reconnecting'&&!s.prolonged));
    if(bannerHead&&bannerSub) {say(bannerHead,title??'');say(bannerSub,s.outboxPurgeFailed?"Couldn't clear unsent messages on this device":sub??'');}
    const questions=cards(s), n=eligiblePicks(s).length, run=s.keepAll;
    say(needsTitle,questions.length?`Needs you (${questions.length})`:'Needs you');
    bulk.hidden=n<2||!!run?.running;bulk.disabled=busy||s.phase!=='live';say(bulk,keepLabel(n));
    bulkStatus.hidden=!run;
    say(bulkStatus,run?(run.running?`Keeping agents' picks: ${run.checked} of ${run.total} checked.`:`Kept ${run.kept} of ${run.total} picks.`):'');
    if(questions.length) list(cardBox,questions,q=>q.id,card,q=>JSON.stringify([actionSig(q),conversation(q)]));
    else empty(cardBox,s.listSeq===null?'':'Nothing needs you right now.');
    const stranded=s.pane==='home'?s.outbox.filter(o=>o.failed&&!o.done):[];
    recovery.hidden=!stranded.length;
    say(recoveryTitle,`${stranded.length} ${stranded.length===1?'message':'messages'} not sent`);
    list(recoveryRows,stranded,localKey,o=>pendingPost(recoveryRows,o),o=>JSON.stringify([o,s.channels.find(c=>c.id===o.channel)?.members]));
    const grouped=new Map<string,ChannelRow[]>();
    for(const c of s.channels) {
      let key:string=c.section==='system'?'direct':c.section;
      if(!c.writable) {
        if(c.kind!=='direct'||c.section==='system'||c.members.length!==2) continue;
        const [a,b]=c.members.map(m=>m.address.match(/^([^/]+)\/[^/]+$/)?.[1]);
        if(!a||!b) continue;
        key=`#${a<b?a:b}`;
      }
      const rows=grouped.get(key);
      if(rows) rows.push(c);else grouped.set(key,[c]);
    }
    if(s.listSeq!==null&&!grouped.size) empty(nav,'No conversations yet.');
    else {
      const sections=[...groups,...[...grouped.keys()].filter(k=>k.startsWith('#')).sort().map(k=>[k,`Behind the scenes · ${k}`] as const)];
      const name=([key]:readonly [string,string])=>key;
      list(nav,sections,name,([key,copy])=>{
        const fold=key.startsWith('#'), section=el(fold?'details':'section');
        section.append(el(fold?'summary':'h2','sect',copy),el('div'));return section;
      },name);
      for(const part of nav.children) if(part.lastElementChild) channelItems(part.lastElementChild,grouped.get((part as HTMLElement).dataset.key??'')??[]);
    }
    const key=scopeKey(s.channel??''), threadKey=scopeKey(s.channel??'',s.thread);
    const labelText=channel?label(channel):'';
    say(channelTitle,labelText||'Welcome to atm chat');
    say(get('channel-people'),channel?channel.members.map(m=>person(m.address,channel)).join(', '):'Choose a conversation to read or reply.');
    older.hidden=!s.histories.get(key)?.older;older.disabled=s.phase!=='live';
    const posts=channel?timeline(s,channel):[], selected=channel&&threadHead(s,channel.id,s.thread);
    const quote=(r:PostRow)=>{
      const parent=threadHead(s,r.channel,r.thread);
      if(!parent) return;
      const excerpt=parent.body.trim().replace(/\s+/g,' ').replace(/^(.{96}).+/u,'$1…')||'Message';
      return r.shared?excerpt:`In reply to ${person(parent.author,channel)}: ${excerpt}`;
    };
    if(!postList(postBox,posts,false,quote)) empty(postBox,channel?(s.histories.has(key)?'No posts yet.':'Loading conversation…'):'Choose a conversation to read or reply.');
    back.hidden=!selected;say(threadTitle,selected?'Thread':labelText?`About ${labelText}`:'Thread');
    say(get('thread-subtitle'),selected?`in ${labelText}`:'');
    if(selected) {
      const shown=postList(replies,[selected,...roots(s,threadKey)],true);
      let end=replies.querySelector('.empty');
      if(shown<2) { if(!end) {end=el('p','empty');replies.append(end);} say(end,s.histories.has(threadKey)?'No replies yet.':'Loading conversation…'); }
      else end?.remove();
    } else {
      const info=el('div','info'); if(channel){info.append(el('h3','','Members'));const people=el('ul');people.append(...channel.members.map(m=>el('li','',person(m.address,channel))));info.append(people);}
      replies.replaceChildren(info);
    }
    if(opened&&!opened.row.isConnected) closeActions();
    const header=port().previousElementSibling;
    if(divider.isConnected&&header!==jumpNew.parentElement) header?.append(jumpNew);
    const activity=[...s.activity??[]].map(([seat,status])=>`${person(seat,channel)} ${status==='working'?'is working…':'needs you at the computer'}`).join(' · ');
    activityLines.forEach((node,i)=>say(node,s.pane===(i?'thread':'channel')?activity:''));
    channelForm.paint(key,`Message ${labelText}`,!!channel,!!channel?.writable);
    threadForm.paint(threadKey,'Reply in thread',!!selected,!!channel?.writable);
    sidePos.afterPatch('home');needsPos.afterPatch('needs');channelPos.afterPatch(key,entry);threadPos.afterPatch(selected?threadKey:`about:${key}`,entry);
    if(s.focus&&s.focus.serial!==focused&&channelLog.clientHeight) {
      const match=[...postBox.children].find(e=>(e as HTMLElement).dataset.postId===s.focus?.id);
      if(match instanceof HTMLElement) {focused=s.focus.serial;match.dataset.focus='true';match.focus({preventScroll:true});
        channelLog.scrollTop+=match.getBoundingClientRect().top-channelLog.getBoundingClientRect().top-channelLog.clientHeight/3;channelPos.beforePatch();}
    }
    for(const row of postBox.children) if(row instanceof HTMLElement&&row.dataset.focus&&!s.focus) delete row.dataset.focus;
    if(viewer.open&&(departed||s.phase==='identity')) {viewer.close();clearViewer();}
    const action=confirmation?.action;
    if(confirmation && (departed||s.phase!=='live'||(action&&!matchesAction(questions.find(q=>q.id===action.id),action)))) dismiss();
    if(field?.isConnected&&document.activeElement!==field) field.focus({preventScroll:true});
    if(s.pane==='settings'&&previousPane!=='settings') settingsBack.focus({preventScroll:true});
    if(previousPane==='settings'&&!settingsOpen.hidden) settingsOpen.focus({preventScroll:true});
    readSoon();
  }
  return {render,bookmarks:()=>[sidePos,channelPos,threadPos,needsPos].map(p=>p.marks()),dispose(){wide.removeEventListener('change',moveNeeds);desktop.removeEventListener('change',backLabel);
    pull.dispose();press.dispose();closeActions(false);sheet.remove();
    clearViewer();if(confirmation) dismiss();
    for(const p of [sidePos,needsPos,channelPos,threadPos]) p.dispose();
    for(const log of logs) log.removeEventListener('scroll',readSoon);reader.disconnect();cancelAnimationFrame(reading);
    document.fonts.removeEventListener('loadingdone',fitFonts);}};
}

const interactive='a,button,input,textarea,select,summary,[contenteditable],.author[tabindex]';

export function bindPostPress(app:HTMLElement,open:(row:HTMLElement)=>boolean,
  clock:Pick<Window,'setTimeout'|'clearTimeout'>=window) {
  const doc=app.ownerDocument, lifetime=new AbortController(), capture={capture:true,signal:lifetime.signal};
  let pending:{id:number;x:number;y:number;row:HTMLElement;timer:number}|undefined;
  let claimed:{id:number;row:HTMLElement}|undefined;
  function cancel() {
    if(pending) clock.clearTimeout(pending.timer);
    pending=undefined;
  }
  const selected=()=>doc.getSelection()?.isCollapsed===false;
  function take(row:HTMLElement,id:number) {
    cancel();if(selected()||!open(row)) return false;
    claimed={id,row};return true;
  }
  const rowAt=(e:Event)=>{
    const target=e.target;
    if(!(target instanceof Element)||target.closest(interactive)||selected()) return;
    const row=target.closest<HTMLElement>('.msg');
    return row&&app.contains(row)?row:undefined;
  };
  doc.addEventListener('pointerdown',e=>{
    cancel();claimed=undefined;
    if(e.pointerType!=='touch'||!e.isPrimary) return;
    const row=rowAt(e);if(!row) return;
    const id=e.pointerId;
    pending={id,row,x:e.clientX,y:e.clientY,timer:clock.setTimeout(()=>take(row,id),500)};
  },capture);
  doc.addEventListener('pointermove',e=>{
    if(pending?.id===e.pointerId&&Math.hypot(e.clientX-pending.x,e.clientY-pending.y)>8) cancel();
  },capture);
  const finish=(e:PointerEvent)=>{if(pending?.id===e.pointerId) cancel();};
  doc.addEventListener('pointerup',finish,capture);
  doc.addEventListener('pointercancel',finish,capture);
  doc.addEventListener('scroll',cancel,capture);
  doc.addEventListener('visibilitychange',cancel,capture);
  doc.addEventListener('selectionchange',()=>{if(selected()) cancel();},capture);
  // Let native selection win if it starts before the sheet. Never suppress a link hold.
  doc.addEventListener('selectstart',e=>{
    if(claimed&&e.target instanceof Node&&claimed.row.contains(e.target)) e.preventDefault();
    else cancel();
  },capture);
  doc.addEventListener('contextmenu',e=>{
    if(e.pointerType==='touch') {
      const hold=pending; // Some browsers deliver their own hold before our timer.
      if(claimed?.id===e.pointerId||(hold?.id===e.pointerId&&hold.row===rowAt(e)&&take(hold.row,hold.id))) e.preventDefault();
      else cancel();
      return;
    }
    const row=e.button===2&&rowAt(e);
    if(row&&open(row)) e.preventDefault();
  },capture);
  // The synthesized click can hit the new modal/backdrop, not the touched row.
  doc.addEventListener('click',e=>{
    if(e.pointerType==='touch'&&claimed?.id===e.pointerId) {
      claimed=undefined;e.preventDefault();e.stopImmediatePropagation();
    }
  },capture);
  return {cancel,dispose(){cancel();lifetime.abort();}};
}

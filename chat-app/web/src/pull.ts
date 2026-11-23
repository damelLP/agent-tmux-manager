import type { PullSource } from './state.ts';

// EDGE_PX leaves room for Android's native Back gesture.
const AXIS_SLOP_PX=8, PULL_PX=64, EDGE_PX=24;
export function classifyPull(dx:number,dy:number) {
  const x=Math.abs(dx);
  return Math.max(x,Math.abs(dy))<AXIS_SLOP_PX?'wait':dy<=x?'cancel':dy<PULL_PX?'pull':'ready';
}

export function bindPull(side:HTMLElement,channel:HTMLElement,refresh:(source:PullSource)=>void) {
  let start:{touch:Touch;port:HTMLElement}|undefined;
  const cancel=()=>{start=undefined;};
  function track(e:TouchEvent) {
    const t=e.changedTouches.item(0);
    if(e.type==='touchcancel') {cancel();return;}
    if(e.type==='touchstart') {
      cancel();
      const target=e.target, port=target instanceof Node?(side.contains(target)?side:channel.contains(target)?channel:null):null;
      if(t&&e.touches.length===1&&port?.clientHeight&&port.scrollTop<=0&&Math.min(t.clientX,innerWidth-t.clientX)>=EDGE_PX) start={touch:t,port};
      return;
    }
    const end=e.type==='touchend';
    if(!start||!t||t.identifier!==start.touch.identifier||e.touches.length!==(end?0:1)||
      !start.port.clientHeight||start.port.scrollTop>0) {cancel();return;}
    const {touch,port}=start, phase=classifyPull(t.clientX-touch.clientX,t.clientY-touch.clientY);
    if(end||phase==='cancel') cancel();
    if(end&&phase==='ready') refresh(port===side?'side':'channel');
  }
  const tracked=['touchstart','touchmove','touchend','touchcancel'] as const;
  // The scrollports already contain vertical overscroll. Observe without claiming native gestures.
  for(const name of tracked) document.addEventListener(name,track,{passive:true});
  return {cancel,dispose(){for(const name of tracked) document.removeEventListener(name,track);}};
}

export type Bookmark = {id:string;offset:number} | 'bottom';
export function preserveScroll(scroller:HTMLElement, defaultBottom=true, saved?:unknown) {
  const marks = new Map<string,Bookmark>(), entries=new Map<string,Bookmark>();
  if(Array.isArray(saved)) for(const pair of saved) {
    if(!Array.isArray(pair)) continue;
    const [key,mark]=pair;
    if(typeof key==='string'&&(mark==='bottom'||(typeof mark?.id==='string'&&Number.isFinite(mark.offset)))) marks.set(key,mark);
  }
  let key = '', dimensions = '', entry:string|undefined;
  const size = () => `${scroller.clientWidth}:${scroller.clientHeight}`;
  const rows = () => [...scroller.querySelectorAll<HTMLElement>('[data-scroll-id]')];
  function save() {
    if (!key || !scroller.clientHeight || dimensions !== size()) return;
    if (defaultBottom && scroller.scrollHeight-scroller.scrollTop-scroller.clientHeight <= 2) { remember('bottom'); return; }
    const top=scroller.getBoundingClientRect().top;
    const first=rows().find(r=>r.getBoundingClientRect().bottom>top);
    if (first?.dataset.scrollId) remember({id:first.dataset.scrollId,offset:first.getBoundingClientRect().top-top});
  }
  function remember(mark:Bookmark) {marks.set(key,mark);if(entry) entries.set(entry,mark);}
  function restore() {
    dimensions=size();
    if (!key || !scroller.clientHeight) return;
    const mark=(entry?entries.get(entry):undefined)??marks.get(key);
    if (mark === 'bottom' || (!mark && defaultBottom)) scroller.scrollTop=scroller.scrollHeight;
    else if (mark) {
      const row=rows().find(r=>r.dataset.scrollId === mark.id);
      if (row) scroller.scrollTop+=row.getBoundingClientRect().top-scroller.getBoundingClientRect().top-mark.offset;
    } else scroller.scrollTop=0;
    save();
  }
  const onResize = new ResizeObserver(restore);
  onResize.observe(scroller);
  scroller.addEventListener('scroll',save,{passive:true});
  return {beforePatch:save,afterPatch(next:string,id?:number){key=next;entry=id===undefined?undefined:`${id}:${next}`;restore();},
    clear(){key='';marks.clear();entries.clear();},marks(){save();return [...marks];},
    dispose(){onResize.disconnect();scroller.removeEventListener('scroll',save);}};
}

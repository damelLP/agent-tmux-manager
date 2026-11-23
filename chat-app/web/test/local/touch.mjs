const finger=(x,y,id=1)=>({x,y,id});
export async function touchDrag(cdp,x,y,dx,dy,{cancel=false,multi=false,retreat=false,beforeEnd}={}) {
  const touch=(type,touchPoints)=>cdp.send('Input.dispatchTouchEvent',{type,touchPoints});
  const at=f=>finger(x+dx*f,y+dy*f);
  await touch('touchStart',[at(0)]);
  if(multi) await touch('touchStart',[at(0),finger(x+26,y+12,2)]);
  for(const f of [0.25,0.5,1]) await touch('touchMove',[at(f),...(multi?[finger(x+26,y+12,2)]:[])]);
  if(retreat) await touch('touchMove',[at(0.45)]);
  await beforeEnd?.();
  await touch(cancel?'touchCancel':'touchEnd',[]);
}

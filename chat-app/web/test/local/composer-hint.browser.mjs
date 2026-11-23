import { afterAll, beforeAll, expect, test } from 'vitest';
import { frame, launch, openChannel, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});

const phone={viewport:{width:360,height:787},deviceScaleFactor:3,isMobile:true,hasTouch:true};
const dm='demo/lead+owner', project='the-long-project-channel-name-needs-an-ellipsis';
function longNames(fake) {
  const call=fake.call;
  fake.call=(method,params)=>{
    if(method==='channel.history'&&params.channel===project) {
      const {seq}=call(method,{...params,channel:'demo'});
      return {seq,channel:project,posts:[],more:false};
    }
    const result=call(method,params);
    if(method==='channel.list') {
      const direct=result.channels.find(c=>c.id===dm), sample=result.channels.find(c=>c.id==='demo');
      direct.members=direct.members.map(m=>m.address==='owner'?m:{...m,address:'agent-tmux-manager/lead'});
      result.channels.push({...sample,id:project,project,lead:`${project}/lead`,last_post:null,last_activity:null});
    }
    return result;
  };
}

const measure=input=>input.evaluate(box=>({height:box.clientHeight,below:box.scrollHeight-box.clientHeight,
  beyond:box.scrollWidth-box.clientWidth}));
const paintedHint=(page,copy)=>page.locator('#channel-form').getByText(copy,{exact:true});

test.each([[dm,'Message agent-tmux-manager lead'],[project,'Message #the-long-project-channel-name-needs-an-ellipsis']])(
  'the empty composer hint fits on the cover screen in %s', (id,copy)=>withChat(browser,async(page,server)=>{
    longNames(server.fake);
    await openChannel(page,server,id);await frame(page);
    const input=page.getByRole('textbox',{name:copy,exact:true});
    expect(await input.getAttribute('placeholder')).toBe(copy);
    const {below,beyond}=await measure(input);
    expect({below,beyond}).toEqual({below:0,beyond:0});
    const hint=paintedHint(page,copy);
    expect(await hint.isVisible()).toBe(true);
    expect(await hint.getAttribute('aria-hidden')).toBe('true');
    expect(await hint.evaluate(el=>{
      const style=getComputedStyle(el);
      const contentHeight=el.getBoundingClientRect().height-parseFloat(style.paddingTop)-parseFloat(style.paddingBottom);
      return {lines:Math.round(contentHeight/parseFloat(style.lineHeight)),overflow:el.scrollWidth>el.clientWidth,
        truncation:style.textOverflow};
    })).toEqual({lines:1,overflow:true,truncation:'ellipsis'});
  },phone));

test('a one-line hint does not stop a typed draft from wrapping and growing',()=>withChat(browser,async(page,server)=>{
  longNames(server.fake);
  await openChannel(page,server,dm);await frame(page);
  const copy='Message agent-tmux-manager lead', input=page.getByRole('textbox',{name:copy,exact:true});
  const empty=(await measure(input)).height;
  await input.fill('A draft with enough words to wrap on the cover screen, without any explicit line breaks.');await frame(page);
  const {height,below,beyond}=await measure(input);
  expect(height).toBeGreaterThan(empty);
  expect({below,beyond}).toEqual({below:0,beyond:0});
  expect(await paintedHint(page,copy).isVisible()).toBe(false);
  await input.fill('');await frame(page);
  expect((await measure(input)).height).toBe(empty);
  expect(await paintedHint(page,copy).isVisible()).toBe(true);
},phone));

import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { expect } from 'vitest';
import { rig } from './rig.mjs';

export async function launch() {
  const {chromium}=await import(pathToFileURL(process.env.ATM_CHAT_TEST_PLAYWRIGHT));
  return chromium.launch({args:['--disable-background-networking','--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1']});
}
export async function withChat(browser, run, options={}) {
  const server=await rig(resolve(process.env.ATM_CHAT_TEST_DIST??'dist'));
  let context;
  try {
    context=await browser.newContext({viewport:{width:412,height:915}, timezoneId:'UTC', serviceWorkers:'block', ...options});
    const page=await context.newPage(), errors=[], outside=[], csp=[];
    context.on('request',r=>{if(!r.url().startsWith(`${server.origin}/`)) outside.push(r.url());});
    page.on('pageerror',e=>errors.push(e.message));
    await context.exposeBinding('recordMessageCsp',(_source,directive)=>csp.push(directive));
    await context.addInitScript(()=>document.addEventListener('securitypolicyviolation',e=>window.recordMessageCsp(e.violatedDirective)));
    if(!await page.evaluate(()=>navigator.onLine)) {
      const cdp=await context.newCDPSession(page);
      await cdp.send('Network.overrideNetworkState',{offline:false,latency:0,downloadThroughput:-1,uploadThroughput:-1});
    }
    await run(page,server);
    expect({errors,csp,outside}).toEqual({errors:[],csp:[],outside:[]});
  } finally {try {await context?.close();} finally {await server.close();}}
}
export const dom=(page,selector)=>page.evaluate(selector=>new Promise(done=>{
  const observer=new MutationObserver(check);
  function check(){if(document.querySelector(selector)) {observer.disconnect();done();}}
  observer.observe(document,{childList:true,subtree:true,attributes:true,characterData:true});check();
}),selector);
export const frame=page=>page.evaluate(async()=>{
  await document.fonts.ready;
  await new Promise(done=>requestAnimationFrame(()=>requestAnimationFrame(done)));
});
export const postSelector=(id,list='posts')=>`#${list} article[data-post-id="${id}"]`;
export async function openChannel(page,server,id='demo') {
  await page.goto(server.origin);
  const item=`#app[data-phase="live"] #channels [data-channel="${id}"]`;
  await dom(page,item);await page.locator(item).click();
}
export async function openThread(page,id) {
  const count=`${postSelector(id)} .reply-count`;
  await dom(page,count);await page.locator(count).click();
}
export async function waitForPost(page,id,list) {
  const selector=postSelector(id,list);
  await dom(page,selector);return page.locator(selector);
}

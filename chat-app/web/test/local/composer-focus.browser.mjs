import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { afterAll, beforeAll, expect, test } from 'vitest';
import { rig } from './rig.mjs';

let browser, server;
beforeAll(async()=>{
  const {chromium}=await import(pathToFileURL(process.env.ATM_CHAT_TEST_PLAYWRIGHT));
  server=await rig(resolve(process.env.ATM_CHAT_TEST_DIST ?? 'dist'));
  browser=await chromium.launch({args:['--disable-background-networking','--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1']});
});
afterAll(async()=>{
  try {await browser?.close();} finally {await server?.close();}
});

test.each(['Enter','Space'])('keyboard arrow submit with %s keeps focus in the composer after the receipt',async key=>{
  const context=await browser.newContext({viewport:{width:840,height:760}});
  try {
    const page=await context.newPage();
    if(!await page.evaluate(()=>navigator.onLine)) {
      const cdp=await context.newCDPSession(page);
      await cdp.send('Network.overrideNetworkState',{offline:false,latency:0,downloadThroughput:-1,uploadThroughput:-1});
    }
    await page.goto(server.origin);
    const channel='#app[data-phase=live] #channels [data-channel="demo"]';
    await page.evaluate(selector=>new Promise(done=>{
      const observer=new MutationObserver(check);
      function check(){if(document.querySelector(selector)) {observer.disconnect();done();}}
      observer.observe(document,{attributes:true,childList:true,subtree:true});check();
    }),channel);
    await page.locator(channel).click();
    const draft=`Keyboard arrow submit via ${key}`;
    const input=page.locator('#channel-form textarea'), arrow=page.locator('#channel-form button[type=submit]');
    await input.fill(draft);await page.keyboard.press('Tab');
    expect(await arrow.evaluate(b=>b===document.activeElement)).toBe(true);
    await page.keyboard.press(key);

    const focus=await page.evaluate(text=>new Promise(done=>{
      const form=document.getElementById('channel-form'), input=form.querySelector('textarea');
      const observer=new MutationObserver(check);
      function check(){
        if(input.readOnly||input.value||![...document.querySelectorAll('#posts .body')].some(p=>p.textContent===text)) return;
        observer.disconnect();requestAnimationFrame(()=>done({inComposer:form.contains(document.activeElement),tag:document.activeElement.tagName}));
      }
      observer.observe(document.getElementById('app'),{attributes:true,childList:true,subtree:true});check();
    }),draft);
    expect(focus).toEqual({inComposer:true,tag:'TEXTAREA'});
  } finally {await context.close();}
});

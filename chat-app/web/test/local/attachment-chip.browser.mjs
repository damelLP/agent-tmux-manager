import { cp, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, launch, openChannel, waitForPost, withChat } from './message-rig.mjs';

const path='/fixture/report.pdf', url='/file?p=%2Ffixture%2Freport.pdf', payload='fixture report bytes';
let browser, assets;
beforeAll(async()=>{
  assets=await mkdtemp(join(tmpdir(),'chat-chip-'));
  await cp(process.env.ATM_CHAT_TEST_DIST??'dist',assets,{recursive:true});
  // Chromium's download GET reaches the server without going through page.route.
  await writeFile(join(assets,'file'),payload);
  browser=await launch();
});
afterAll(async()=>{try{await browser?.close();}finally{if(assets) await rm(assets,{recursive:true});}});
test.each([
  [503,"Couldn't check this file.",true],
  ['network',"Couldn't check this file.",true],
  [403,'On the computer only',false],
  [404,'No longer there',false],
])('a HEAD %s leaves a download retry only for a transient failure', (outcome,detail,retry)=>withChat(browser,async(page,server)=>{
  const post=server.fake.inject('demo','An attached report',{attachments:[path]});
  server.publish(assets);
  await page.route(server.origin+url,route=>route.request().method()!=='HEAD'?route.continue():
    outcome==='network'?route.abort():route.fulfill({status:outcome}));
  await openChannel(page,server);
  const row=await waitForPost(page,post.id);
  await row.locator('.file-chip small').evaluate((node,want)=>new Promise(resolve=>{
    const observer=new MutationObserver(check);
    function check(){if(node.textContent===want){observer.disconnect();resolve();}}
    observer.observe(node,{childList:true,characterData:true,subtree:true});check();
  }),detail);
  const link=row.getByRole('link',{name:/^report\.pdf/});
  expect(await link.count()).toBe(Number(retry));
  if(!retry) {
    expect(await row.getByRole('button',{name:/^report\.pdf/}).isDisabled()).toBe(true);
    return;
  }
  expect(await link.getAttribute('href')).toBe(url);
  const before=page.url();
  const [download]=await Promise.all([page.waitForEvent('download'),link.tap()]);
  expect(download.suggestedFilename()).toBe('report.pdf');
  expect(await readFile(await download.path(),'utf8')).toBe(payload);
  expect(page.url()).toBe(before);
},{hasTouch:true,acceptDownloads:true}));

function reportShown(page, phrase) {
  return page.evaluate(phrase=>new Promise(done=>{
    const viewer=document.querySelector('#photo-view');
    const observer=new MutationObserver(check);
    function check() {
      if(viewer.open&&[...viewer.children].some(n=>!n.hidden&&n.textContent.includes(phrase))) {
        observer.disconnect();done();
      }
    }
    observer.observe(viewer,{childList:true,characterData:true,attributes:true,subtree:true});check();
  }),phrase);
}

test('formats the Markdown attachment, leaves other text literal, and downloads HTML',()=>withChat(browser,async(page,server)=>{
  const markdown='/fixture/build notes.MD', log='/fixture/build.log', html='/fixture/report.html';
  const text='# Build notes\n\n- **3 checks passed**\n- `npm test`\n\n<b>Literal markup</b>\n\n'+
    '![Diagram](/diagram.png)\n\n[Reference](https://fixture.invalid/reference)\n';
  const document='<p>Saved HTML report</p>';
  const post=server.fake.inject('demo','Build files',{attachments:[markdown,log,html]});
  server.control.http=(req,res)=>{
    const url=new URL(req.url,server.origin), file=url.searchParams.get('p');
    if(url.pathname!=='/file'||![markdown,log,html].includes(file)) return false;
    const body=file===html?document:text;
    res.writeHead(200,{'Content-Type':file===html?'application/octet-stream':'text/plain; charset=utf-8',
      'Content-Length':Buffer.byteLength(body),'Cache-Control':'no-store'}).end(req.method==='HEAD'?undefined:body);
    return true;
  };
  await openChannel(page,server);
  const row=await waitForPost(page,post.id), viewer=page.getByRole('dialog');
  await row.getByRole('button',{name:/^build notes\.MD/}).click();
  await reportShown(page,'3 checks passed');
  expect(await viewer.locator('strong').allTextContents()).toEqual(['Build notes','3 checks passed']);
  expect(await viewer.locator('ul li').allTextContents()).toEqual(['3 checks passed','npm test']);
  expect(await viewer.locator('code').textContent()).toBe('npm test');
  expect(await viewer.textContent()).toContain('<b>Literal markup</b>');
  expect(await viewer.textContent()).toContain('![Diagram](/diagram.png)');
  expect(await viewer.locator('b, img:not([hidden]), iframe').count()).toBe(0);
  const link=viewer.getByRole('link',{name:'Reference'});
  expect(await link.getAttribute('href')).toBe('https://fixture.invalid/reference');
  expect(await link.getAttribute('rel')).toBe('noopener noreferrer');
  expect(server.http.some(req=>req.url==='/diagram.png')).toBe(false);

  await viewer.getByRole('button',{name:'Close'}).click();
  await row.getByRole('button',{name:/^build\.log/}).click();
  await reportShown(page,'3 checks passed');
  const plain=viewer.locator('pre:visible');
  expect(await plain.textContent()).toBe(text);
  expect(await plain.locator('*').count()).toBe(0);
  expect(await viewer.getByRole('link',{name:'Reference'}).count()).toBe(0);
  await viewer.getByRole('button',{name:'Close'}).click();
  await dom(page,`[data-post-id="${post.id}"] a[download="report.html"]`);
  const before=page.url();
  const [download]=await Promise.all([page.waitForEvent('download'),row.getByRole('link',{name:/^report\.html/}).click()]);
  expect(download.suggestedFilename()).toBe('report.html');
  expect(await readFile(await download.path(),'utf8')).toBe(document);
  expect(page.url()).toBe(before);
},{acceptDownloads:true}));

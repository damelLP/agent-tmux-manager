import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, frame, launch, openChannel, openThread, postSelector, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const direct='demo/lead+owner';
const fence=text=>`\`\`\`text\n${text}\n\`\`\``;
const markup=['## Review *ready*','**Passed @owner** in the *chat smoke*.','> Follow up in the thread.','- channel','* direct',
  '3. Run `npm test`','4. Read [the report](http://example.test/report)','Reference: https://example.test/smoke.',
  fence('  @owner\t\n**literal**')].join('\n');

test.each(['main','thread','DM'])('renders the report in %s',scope=>withChat(browser,async(page,server)=>{
  const channel=scope==='DM'?direct:'demo';
  const root=scope!=='main'&&server.fake.inject(channel,'Literal <b>root</b>');
  const report=server.fake.inject(channel,markup+(scope==='main'?'\n@demo/chief':''),{thread:root?.id??null,mentions:['owner'],kind:'report'});
  await openChannel(page,server,channel);
  if(scope==='thread') await openThread(page,root.id);
  const row=await waitForPost(page,report.id,scope==='thread'?'replies':'posts'), body=row.locator('.body');
  expect(await body.locator('blockquote').allTextContents()).toEqual(['Follow up in the thread.']);
  expect(await body.locator('ul > li').allTextContents()).toEqual(['channel','direct']);
  expect(await body.locator('ol[start="3"] > li').allTextContents()).toEqual(['Run npm test','Read the report']);
  expect(await body.locator('strong').allTextContents()).toEqual(['Review ready','Passed @owner']);
  expect(await body.locator('strong em').allTextContents()).toEqual(['ready']);
  expect(await body.locator('h1,h2,h3').count()).toBe(0);
  expect(await body.locator('strong').first().evaluate(el=>getComputedStyle(el).fontSize)).toBe(await body.evaluate(el=>getComputedStyle(el).fontSize));
  expect(await body.locator('em').allTextContents()).toEqual(['ready','chat smoke']);
  expect(await body.locator('code').allTextContents()).toEqual(['npm test','  @owner\t\n**literal**']);
  expect(await body.locator('strong .mention').allTextContents()).toEqual(['@owner']);
  expect(await body.locator('.mention').allTextContents()).toEqual(scope==='main'?['@owner','@demo/chief (closed)']:['@owner']);
  expect(await body.locator('a').evaluateAll(links=>links.map(a=>[a.textContent,a.getAttribute('href'),a.target,a.rel]))).toEqual([
    ['the report','http://example.test/report','_blank','noopener noreferrer'],
    ['https://example.test/smoke','https://example.test/smoke','_blank','noopener noreferrer'],
  ]);
  if(scope==='DM') {
    expect(await row.locator('.quote').textContent()).toBe('In reply to demo lead: Literal <b>root</b>');
    expect(await row.locator('.quote b').count()).toBe(0);
  }
}));

test('keeps tag-shaped text and rejected links literal in formatted output',()=>withChat(browser,async(page,server)=>{
  const literal='<script type="text/plain">marker</script> <b>raw</b> <img data-note="sample">';
  const refused='[x](javascript:literal) [x](data:text/plain,literal) [x](//example.test) [x](http:example.test)';
  const note=server.fake.inject('demo',`**${literal}**\n${refused}\n[valid](https://example.test)\n***odd***`);
  await openChannel(page,server);
  const row=await waitForPost(page,note.id), body=row.locator('.body');
  expect(await body.locator('script,b,img,[data-note]').count()).toBe(0);
  expect(await body.textContent()).toContain(literal);
  expect(await body.textContent()).toContain(refused);
  expect(await body.textContent()).toContain('***odd***');
  expect(await body.locator('strong').allTextContents()).toEqual([literal]);
  expect(await body.getByRole('link').evaluateAll(links=>links.map(a=>a.protocol))).toEqual(['https:']);
}));

test.each([false,true])('long snippet is a keyboard/touch disclosure (touch: %s)',hasTouch=>withChat(browser,async(page,server)=>{
  const short=Array(12).fill('short').join('\n'), long=['x'.repeat(1_225),...Array(12).fill('proof is text')].join('\n');
  const a=server.fake.inject('demo',fence(short)), b=server.fake.inject('demo',fence(long));
  await openChannel(page,server);
  const row=await waitForPost(page,b.id), first=page.locator(postSelector(a.id));
  expect(await first.locator('summary').count()).toBe(0);
  expect(await first.locator('pre').isVisible()).toBe(true);
  const details=row.locator('details'), summary=row.locator('summary'), pre=row.locator('pre');
  expect(await details.count()).toBe(1);
  expect(await summary.textContent()).toBe('Show more · 13 lines');
  expect(await pre.isVisible()).toBe(false);
  expect(await details.ariaSnapshot()).not.toContain('proof is text');
  if(hasTouch) await summary.tap();
  else {await summary.focus();await page.keyboard.press('Enter');}
  await dom(page,`${postSelector(b.id)} details[open]`);await frame(page);
  expect(await pre.isVisible()).toBe(true);
  expect(await pre.locator('code').textContent()).toBe(long);
  expect(await pre.evaluate(el=>el.tabIndex===0&&el.scrollWidth>el.clientWidth)).toBe(true);
  expect(await page.locator('#channel-log').evaluate(el=>el.scrollWidth===el.clientWidth)).toBe(true);
  expect(await row.getAttribute('data-reveal')).toBeNull();
  await summary.evaluate(el=>{el.focus({preventScroll:true});window.savedSnippet=el.parentElement;});
  const next=server.fake.inject('demo','Unrelated Watch render');
  await waitForPost(page,next.id);await frame(page);
  expect(await summary.evaluate(el=>el===document.activeElement&&el.parentElement===window.savedSnippet&&el.parentElement.open)).toBe(true);
  expect(await summary.textContent()).toBe('Show less');
  if(hasTouch) await summary.tap(); else await page.keyboard.press('Space');
  expect(await pre.isVisible()).toBe(false);
},{hasTouch}));

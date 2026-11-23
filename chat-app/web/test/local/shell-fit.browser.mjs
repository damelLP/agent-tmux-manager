import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, frame, launch, openChannel, postSelector, waitForPost, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
// A folded phone's cover screen between its system bars, and the bars a page may be drawn under.
const phone={viewport:{width:360,height:787},deviceScaleFactor:3,isMobile:true,hasTouch:true};
const insets={top:38,topMax:38,bottom:15,bottomMax:15};

test('a phone shows the whole composer, clear of both safe-area insets',()=>withChat(browser,async(page,server)=>{
  const cdp=await page.context().newCDPSession(page);
  await cdp.send('Emulation.setSafeAreaInsetsOverride',{insets});
  await openChannel(page,server);
  await dom(page,'#channel-form:not([hidden])');await frame(page);
  const fit=await page.evaluate(()=>{
    const box=selector=>document.querySelector(selector).getBoundingClientRect(), root=document.documentElement;
    // The status line under the message box is the composer's lowest content.
    return {pageScroll:root.scrollHeight-root.clientHeight,headerTop:box('.channel>header').top,
      belowComposer:innerHeight-box('#channel-form').bottom,belowStatus:innerHeight-box('#channel-form p').bottom};
  });
  expect(fit.pageScroll).toBe(0);
  expect(fit.headerTop).toBeCloseTo(insets.top,1);
  expect(fit.belowComposer).toBeCloseTo(0,1);
  expect(fit.belowStatus).toBeGreaterThanOrEqual(insets.bottom);
},phone));

// An installed Android app can report viewport units one hidden browser toolbar (56 px) taller than its window: a
// body sized with 100dvh then ended below the screen and cut the composer in half. Blink adds that toolbar to the
// large and dynamic viewport units but not to the layout height, which height:100% follows. Headless Chromium has no
// toolbar and cannot show the difference, so this reads which unit the served style sheet sizes the page shell with.
test('no viewport unit sizes the page shell',()=>withChat(browser,async(page,server)=>{
  await openChannel(page,server);
  const sized=await page.evaluate(()=>{
    const all=rule=>[rule,...[...rule.cssRules??[]].flatMap(all)];
    const shell=rule=>rule.selectorText?.split(',').some(part=>/^(html|body|#app)$/.test(part.trim()));
    const heights=({style})=>['height','min-height','block-size','min-block-size'].map(name=>`${name}: ${style.getPropertyValue(name)}`);
    return [...document.styleSheets].flatMap(sheet=>[...sheet.cssRules]).flatMap(all).filter(shell)
      .flatMap(rule=>heights(rule).filter(size=>/v[hb]\b/.test(size)).map(size=>`${rule.selectorText} { ${size} }`));
  });
  expect(sized).toEqual([]);
}));

// Headless Chromium cannot open an Android keyboard. Check the directive in the built page,
// then independently test what the shell does when Chromium has resized the layout viewport.
test('the served page asks the keyboard to resize the layout viewport',()=>withChat(browser,async(page,server)=>{
  await page.goto(server.origin);
  const viewport=await page.locator('meta[name="viewport"]').getAttribute('content');
  expect(viewport?.split(',').map(part=>part.trim())).toContain('interactive-widget=resizes-content');
},phone));

test('a shorter phone layout keeps the focused composer and latest message in view',()=>withChat(browser,async(page,server)=>{
  const latest=server.fake.inject('demo','Newest message before typing');
  await openChannel(page,server);
  await waitForPost(page,latest.id);
  const input=page.locator('#channel-form textarea');
  await input.fill('Typing a draft');await frame(page);
  const measure=()=>page.evaluate(selector=>{
    const root=document.documentElement, log=document.getElementById('channel-log');
    const history=log.getBoundingClientRect(), last=document.querySelector(selector).getBoundingClientRect();
    const composer=document.getElementById('channel-form').getBoundingClientRect();
    return {height:innerHeight, layoutHeight:root.clientHeight, overflow:root.scrollHeight-root.clientHeight,
      pageScroll:[scrollX,scrollY], composerTop:composer.top, composerBottom:composer.bottom,
      focused:document.activeElement===document.querySelector('#channel-form textarea'),
      scrollable:log.scrollHeight>log.clientHeight, historyGap:log.scrollHeight-log.scrollTop-log.clientHeight,
      latestVisible:last.top>=history.top&&last.bottom<=history.bottom};
  },postSelector(latest.id));
  const closed=await measure();
  expect(closed).toMatchObject({height:787, scrollable:true, latestVisible:true});
  expect(closed.historyGap).toBeLessThanOrEqual(2);

  await page.setViewportSize({width:360,height:420});await frame(page);
  const fit=await measure();
  expect(fit).toMatchObject({height:420, layoutHeight:420, overflow:0, pageScroll:[0,0], focused:true, latestVisible:true});
  expect(fit.composerTop).toBeGreaterThanOrEqual(0);
  expect(fit.composerBottom).toBeLessThanOrEqual(fit.height);
  expect(fit.historyGap).toBeLessThanOrEqual(2);
},phone));

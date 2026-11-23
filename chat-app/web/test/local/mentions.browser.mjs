import { afterAll, beforeAll, expect, test } from 'vitest';
import { dom, launch, openChannel, openThread, withChat } from './message-rig.mjs';

let browser;
beforeAll(async()=>{browser=await launch();});
afterAll(async()=>{await browser?.close();});
const composer=(page,kind='channel')=>page.locator(`#${kind}-form textarea`);

test.each(['channel','thread'])('the %s picker fits above the draft without covering it',async kind=>{
  await withChat(browser,async(page,server)=>{
    const root=server.fake.inject('demo','Fixture thread for the mention picker');
    server.fake.inject('demo','A reply to the fixture thread',{thread:root.id});
    await openChannel(page,server);
    if(kind==='thread') await openThread(page,root.id);
    const input=composer(page,kind), draft='Keep this text.\nAsk @';
    await input.fill(draft);
    const {menu,editor}=await input.evaluate(input=>({
      menu:input.form.querySelector('[role=listbox]').getBoundingClientRect().toJSON(),
      editor:input.closest('.compose-row').getBoundingClientRect().toJSON(),
    }));
    expect(menu.height).toBeGreaterThan(0);
    expect(menu.bottom).toBeLessThanOrEqual(editor.top);
    expect(await input.inputValue()).toBe(draft);
  },{viewport:{width:412,height:540},hasTouch:true});
});

test('predictive composition filters the mention picker without taking IME keys',async()=>{
  await withChat(browser,async(page,server)=>{
    await openChannel(page,server);
    const input=composer(page), picker=page.locator('#channel-form').getByRole('listbox');
    await input.fill('@');
    expect(await picker.isVisible()).toBe(true);
    const cdp=await page.context().newCDPSession(page);
    await input.evaluate(el=>el.addEventListener('input',e=>{el.dataset.composing=String(e.isComposing);},{once:true}));
    await cdp.send('Input.imeSetComposition',{text:'l',selectionStart:1,selectionEnd:1});
    await dom(page,'#channel-form textarea[data-composing]');
    expect(await input.getAttribute('data-composing')).toBe('true');
    expect(await input.inputValue()).toBe('@l');
    expect(await picker.isVisible()).toBe(true);
    expect(await picker.getByRole('option').allTextContents()).toEqual(['@demo/lead']);
    for(const key of ['ArrowDown','Enter']) {
      expect(await input.evaluate((el,key)=>el.dispatchEvent(new KeyboardEvent('keydown',
        {key,bubbles:true,cancelable:true,isComposing:true})),key)).toBe(true);
    }
    expect(await input.inputValue()).toBe('@l');
    await cdp.send('Input.insertText',{text:'l'});
    await input.press('Enter');
    expect(await input.inputValue()).toBe('@demo/lead ');
    expect(server.frames.filter(f=>f.method==='channel.post')).toEqual([]);
  },{hasTouch:true});
});

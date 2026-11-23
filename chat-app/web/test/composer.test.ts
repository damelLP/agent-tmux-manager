import { expect, test } from 'vitest';
import { enterAction, mentionChoices, mentionRange, sendControl } from '../src/composer.ts';

test('empty and whitespace drafts cannot be sent', () => {
  expect(sendControl(true,'').disabled).toBe(true);
  expect(sendControl(true,' \n\t').disabled).toBe(true);
  expect(sendControl(true,'a')).toEqual({disabled:false,retry:false,label:'Send'});
  expect(sendControl(true,'x'.repeat(16_385)).disabled).toBe(false); // Session reports the size error.
});

test('a selected photo enables Send with empty text, but never on a disconnected link',()=>{
  expect(sendControl(true,'',undefined,true).disabled).toBe(false);
  expect(sendControl(false,'',undefined,true).disabled).toBe(true);
  expect(sendControl(true,'','sending',true).disabled).toBe(true);
});

test('an outstanding send waits and an unknown result allows explicit Retry', () => {
  expect(sendControl(true,'a','sending')).toEqual({disabled:true,retry:false,label:'Send'}); // The status line announces progress.
  expect(sendControl(true,'','unknown')).toEqual({disabled:false,retry:true,label:'Retry'});
  expect(sendControl(false,'','unknown').disabled).toBe(true);
  expect(sendControl(false,'a').disabled).toBe(true);
});

test('ranks member mentions before other seats, dedupes, and caps the picker',()=>{
  const channel={members:['owner','demo/worker','demo/lead','system'].map(address=>({address})),former:['demo/closed']};
  const seats=['demo/closed','demo/lead','else/lead','a/one','b/two','c/three','d/four','e/five','bad id','bad/Name'];
  expect(mentionChoices(channel,seats,'')).toEqual(['demo/lead','demo/worker','a/one','b/two','c/three','d/four']);
  expect(mentionChoices(channel,seats,'le')).toEqual(['demo/lead','else/lead']);
  expect(mentionChoices(channel,seats,'else/l')).toEqual(['else/lead']);
  expect(mentionChoices(channel,[],'w')).toEqual(['demo/worker']);
});

test('finds the whole active mention at the caret without touching punctuation or other text',()=>{
  expect(mentionRange('Ask (@demo/lead), then @other',11,11)).toEqual({start:5,end:15,query:'demo/'});
  expect(mentionRange('Ask (@), then',6,6)).toEqual({start:5,end:6,query:''});
  expect(mentionRange('Ask @LE',7,7)).toEqual({start:4,end:7,query:'le'});
  for(const text of ['me@work','foo/@lead','@demo/lead/extra','@lea@other','@café']) {
    const caret=text.indexOf('@')+1;
    expect(mentionRange(text,caret,caret)).toBeUndefined();
  }
  expect(mentionRange('Ask @lead',5,7)).toBeUndefined();
});

const enter={key:'Enter',shiftKey:false,ctrlKey:false,altKey:false,metaKey:false,isComposing:false,keyCode:13,repeat:false,defaultPrevented:false};

test('desktop Enter sends only one eligible fresh draft', () => {
  expect(enterAction(enter,false,true)).toBe('send');
  expect(enterAction(enter,false,false)).toBe('block');
  expect(enterAction({...enter,repeat:true},false,true)).toBe('block');
});

test('touch, composition, and modified Return stay native', () => {
  expect(enterAction(enter,true,true)).toBe('native');
  for (const event of [{shiftKey:true},{ctrlKey:true},{altKey:true},{metaKey:true},{isComposing:true},{keyCode:229},{defaultPrevented:true},{key:'A'}]) {
    expect(enterAction({...enter,...event},false,true)).toBe('native');
  }
});

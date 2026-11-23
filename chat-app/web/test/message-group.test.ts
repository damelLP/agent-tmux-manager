import { expect, it } from 'vitest';
import { continuesMessage } from '../src/message-group.ts';

const first={author:'demo/lead',at:0,day:'today',boundary:false};
it.each([
  [0,true],[299_999,true],[300_000,true],[300_001,false],[-1,false],[NaN,false],[Infinity,false],
] as const)('groups same-author neighbors at gap %s: %s',(at,want)=>{
  expect(continuesMessage(first,{...first,at})).toBe(want);
});
it('breaks on a missing neighbor, a different author/day, invalid predecessor or either boundary',()=>{
  expect(continuesMessage(undefined,first)).toBe(false);
  expect(continuesMessage(first,{...first,author:'demo lead'})).toBe(false);
  expect(continuesMessage(first,{...first,at:1,day:'tomorrow'})).toBe(false);
  expect(continuesMessage({...first,at:NaN},{...first,at:1})).toBe(false);
  expect(continuesMessage({...first,boundary:true},first)).toBe(false);
  expect(continuesMessage(first,{...first,boundary:true})).toBe(false);
});
it('compares each neighbor, not elapsed time since the leading row',()=>{
  const second={...first,at:240_000}, third={...first,at:480_000};
  expect(continuesMessage(first,second)).toBe(true);
  expect(continuesMessage(second,third)).toBe(true);
});

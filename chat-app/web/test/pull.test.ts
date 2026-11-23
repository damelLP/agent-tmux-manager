import { expect, it } from 'vitest';
import { classifyPull } from '../src/pull.ts';

it.each([
  [0,7,'wait'], [8,8,'cancel'], [9,1,'cancel'], [0,-8,'cancel'], [0,63,'pull'], [0,64,'ready'],
])('classifies displacement (%i,%i) as %s',(dx,dy,want)=>{
  expect(classifyPull(dx,dy)).toBe(want);
});

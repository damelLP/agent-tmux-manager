import { expect, it } from 'vitest';
import { parseMessage } from '../src/markup.ts';
import type { Markup } from '../src/markup.ts';

const el = (tag:string,...children:Markup[]) => ({tag,children});
const code = (text:string) => ({tag:'code' as const,text});
const pre = (text:string,lines?:number) => ({tag:'pre',children:[code(text)],lines});
const a = (text:string,href:string) => ({tag:'a' as const,text,href});

it('classifies block runs without losing lines',()=>{
  expect(parseMessage('first\r\nsecond\r> Quote\n> - literal\n- a\n* b\n3. three\n1. four\n  - indented')).toEqual([
    el('div','first\nsecond'), el('blockquote','Quote\n- literal'),
    el('ul',{tag:'li',children:['a']},{tag:'li',children:['b']}),
    {tag:'ol',start:3,children:[el('li','three'),el('li','four')]},el('div','  - indented'),
  ]);
  expect(parseMessage('>\n> > nested\n-word\n1.0')).toEqual([
    el('blockquote','\n> nested'),el('div','-word\n1.0'),
  ]);
  expect(parseMessage('- a\n\n- b').filter(b=>typeof b!=='string'&&b.tag==='ul')).toEqual([
    {tag:'ul',children:[el('li','a')]},{tag:'ul',children:[el('li','b')]},
  ]);
  expect(parseMessage('0. zero\n12. next\n1000000000. not a marker')).toEqual([
    {tag:'ol',start:0,children:[el('li','zero'),el('li','next')]},el('div','1000000000. not a marker'),
  ]);
});

it('makes one bold line per supported hash prefix',()=>{
  expect(parseMessage('# A\n## *B*\n### C\n#### D\n##no-space\n ## indented')).toEqual([
    el('strong','A'),el('strong',{tag:'em',children:['B']}),el('strong','C'),el('div','#### D\n##no-space\n ## indented'),
  ]);
});

it('preserves fenced code and leaves unclosed or unsupported fences literal',()=>{
  expect(parseMessage('```text\r\n  <b>\t\n> x\n\n* y\n```\t')).toEqual([pre('  <b>\t\n> x\n\n* y')]);
  expect(parseMessage('```\n```')).toEqual([pre('')]);
  expect(parseMessage('````\ntext\n````')).toEqual([el('div','````\ntext\n````')]);
  const rest='```ts\n**unfinished**\n- rest';
  expect(parseMessage(`**intro**\n${rest}`)).toEqual([
    el('div',{tag:'strong',children:['intro']}),el('div',rest),
  ]);
});

it.each([
  [Array(12).fill('code').join('\n'),undefined], [Array(13).fill('code').join('\n'),13],
  ['x'.repeat(1_200),undefined], ['x'.repeat(1_201),1],
] as const)('folds only above one of the code thresholds (%#)',(text,collapsedLines)=>{
  expect(parseMessage(`\`\`\`\n${text}\n\`\`\``)).toEqual([pre(text,collapsedLines)]);
});

it('pairs only supported one-line markers with code opaque',()=>{
  expect(parseMessage('**bold _flat_** *em* _again_ ` @owner <b> `')).toEqual([{tag:'div',children:[
    el('strong','bold _flat_'),' ',el('em','em'),' ',el('em','again'),' ',code(' @owner <b> '),
  ]}]);
  expect(parseMessage('**see `*literal*` and [checks](https://example.test)**')).toEqual([{tag:'div',children:[
    el('strong','see ',code('*literal*'),' and ',a('checks','https://example.test/')),
  ]}]);
  const malformed='***odd*** foo_bar_baz * split*\n*one\ntwo* **unclosed\nélève_x_y \n``literal``';
  expect(parseMessage(malformed)).toEqual([el('div',malformed)]);
  const dense='run: '+'* not ** _ '.repeat(1_350);
  expect(parseMessage(dense)).toEqual([el('div',dense)]);
  expect(parseMessage('**unclosed then _em_')).toEqual([{tag:'div',children:['**unclosed then ',el('em','em')]}]);
});

it('links explicit HTTP(S) targets and keeps surrounding punctuation',()=>{
  expect(parseMessage('(https://example.test/a_(b)). [checks](HTTP://example.test) `https://example.test`')).toEqual([el('div',
    '(',a('https://example.test/a_(b)','https://example.test/a_(b)'),'). ',a('checks','http://example.test/'),' ',code('https://example.test'))]);
  const bad='[x](javascript:literal) [x](data:text/plain,literal) [x](//example.test) [x](http:example.test) '
    +'[x](https://user@example.test) ![image](https://example.test/i) [x](https://example.test title) '
    +'[x](http://@example.test) [x](https:///no-host.test) [x](http://example.test\\path) [x](http://example.test/\u0001x) '
    +'[x](https://) [x](https://example.test/\u0085x) [](https://example.test) prehttps://example.test';
  expect(parseMessage(bad)).toEqual([el('div',bad)]);
});

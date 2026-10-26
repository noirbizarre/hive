import { afterEach, expect, test } from 'vitest';
import { mount } from '@vue/test-utils';
import MarkdownMessage from './MarkdownMessage.vue';

let wrapper;
afterEach(() => wrapper?.unmount());

test('discussion Markdown renders semantic prose code and tables', () => {
  wrapper = mount(MarkdownMessage, { props: { text: [
    '# Summary', '', '**Strong** and *emphasis* and ~~removed~~ with `inline()`.',
    'Next line', '', '- First', '  - Nested', '', '1. Ordered', '', '> Quoted', '',
    '[Documentation](https://example.org/docs)', '', '```js', 'const html = "<b>literal</b>";', '```', '',
    '| Name | Value |', '| --- | --- |', '| Item | 42 |', '', '---',
  ].join('\n') } });
  expect(wrapper.get('h1').text()).toBe('Summary');
  expect(wrapper.get('strong').text()).toBe('Strong');
  expect(wrapper.get('em').text()).toBe('emphasis');
  expect(wrapper.get('s').text()).toBe('removed');
  expect(wrapper.get('p code').text()).toBe('inline()');
  expect(wrapper.find('p br').exists()).toBe(true);
  expect(wrapper.get('ul ul li').text()).toBe('Nested');
  expect(wrapper.get('ol li').text()).toBe('Ordered');
  expect(wrapper.get('blockquote').text()).toBe('Quoted');
  expect(wrapper.get('a').attributes('href')).toBe('https://example.org/docs');
  expect(wrapper.get('pre code.language-js').element.textContent).toBe('const html = "<b>literal</b>";\n');
  expect(wrapper.find('pre b').exists()).toBe(false);
  expect(wrapper.findAll('th').map(cell => cell.text())).toEqual(['Name', 'Value']);
  expect(wrapper.findAll('td').map(cell => cell.text())).toEqual(['Item', '42']);
  expect(wrapper.find('hr').exists()).toBe(true);
});

test('untrusted message HTML remains literal and executable links are rejected', () => {
  const html = '<script>alert(1)</script>\n<img src=x onerror="alert(1)">\n<b>literal</b>';
  wrapper = mount(MarkdownMessage, { props: { text: `${html}\n\n[javascript](javascript:alert%281%29)\n[encoded](jav&#x61;script:alert%281%29)\n[vbscript](vbscript:evil)\n[data](data:text/html;base64,abc)\n![unsafe](javascript:evil)` } });
  expect(wrapper.find('script, img, b, a, [onerror]').exists()).toBe(false);
  expect(wrapper.text()).toContain('<script>alert(1)</script>');
  expect(wrapper.text()).toContain('<img src=x onerror="alert(1)">');
  expect(wrapper.text()).toContain('<b>literal</b>');
});

test('accumulated streaming text completes emphasis and code without duplication', async () => {
  wrapper = mount(MarkdownMessage, { props: { text: '**Answer' } });
  expect(wrapper.find('strong').exists()).toBe(false);
  await wrapper.setProps({ text: '**Answer**\n\n```js\nconst value =' });
  expect(wrapper.get('strong').text()).toBe('Answer');
  expect(wrapper.get('pre code').element.textContent).toBe('const value =');
  await wrapper.setProps({ text: '**Answer**\n\n```js\nconst value = 42;\n```\n\nDone.' });
  expect(wrapper.findAll('strong')).toHaveLength(1);
  expect(wrapper.findAll('pre')).toHaveLength(1);
  expect(wrapper.get('pre code').element.textContent).toBe('const value = 42;\n');
  expect(wrapper.findAll('p').at(-1).text()).toBe('Done.');
});

// Browser tab over CDP: AX rendering and pruning against a recorded
// Accessibility.getFullAXTree of test/fixtures/browser-page.js (?items=12),
// locator resolution through the sandbox with a recorded-tree browser double,
// input validation, and (only with CUA_BROWSER_CDP set) a real browser.
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { capLines, rectsFromSnapshot, renderAxTree } from '../src/browser/ax.js';
import { BrowserHost, checkUrl, parseKey } from '../src/browser/browser.js';
import { parseEndpoint } from '../src/browser/cdp.js';
import { SandboxRunner } from '../src/sandbox/runner.js';
import { startServer } from './fixtures/browser-page.js';

const FIXTURE = JSON.parse(readFileSync(new URL('./fixtures/browser-ax.json', import.meta.url), 'utf8'));
const render = (snapshot = FIXTURE.snapshot) => renderAxTree(FIXTURE.nodes, { rects: rectsFromSnapshot(snapshot), viewport: FIXTURE.viewport });
const NO_ENGINE = { methods: [], call: async () => { throw new Error('no engine in this test'); } };

describe('browser AX rendering', () => {
  const { lines, elements } = render();
  const text = lines.join('\n');
  const line = (re) => lines.find((l) => re.test(l));

  test('form controls render like native elements, with values as text and no inner editor nodes', () => {
    assert.match(line(/textbox "Name"/), /^\s+\[\d+\] textbox "Name" \(editable\) text="old"$/);
    const name = lines.indexOf(line(/textbox "Name"/));
    assert.doesNotMatch(lines[name + 1], /"old"/, 'the input\'s inner editor is not shown again');
    assert.match(line(/combobox "Color"/), /combobox "Color" \(collapsed\) text="Red"$/);
    assert.match(line(/option "Red"/), /option "Red" \(selected\)/);
    assert.match(line(/checkbox/), /checkbox "Gift wrap" \(unchecked\)$/);
    assert.match(line(/textbox "Note"/), /textbox "Note" \(editable\)$/);
    assert.ok(line(/button "Send order"$/) && line(/button "Save for later"$/));
  });

  test('wrappers, ignored nodes, inline text boxes and repeated names are pruned', () => {
    assert.doesNotMatch(text, /\] (generic|none|InlineTextBox|inline text box|list marker|menu list popup)\b/);
    assert.ok(!lines.some((l) => /static text "(Home|Help|Send order|open item 3)"/.test(l)), 'link and button text is not repeated');
    // Every line is one element; ids are the DOM nodes' backend ids.
    const ids = new Set(FIXTURE.nodes.map((n) => n.backendDOMNodeId));
    assert.equal(lines.length, elements.length);
    for (const el of elements) assert.ok(ids.has(el.id), `id ${el.id} is a backendDOMNodeId`);
    assert.ok(lines.length < 80, `pruned to ${lines.length} lines from ${FIXTURE.nodes.length} nodes`);
  });

  test('list rows keep their text and link in order; text-only containers show text=', () => {
    const i = lines.findIndex((l) => /static text "Item 3"/.test(l));
    assert.ok(i > 0);
    assert.match(lines[i - 1], /\] listitem @/);
    assert.match(lines[i + 1], /link "open item 3"$/);
    assert.match(lines[i + 2], /static text "updated 3 days ago"$/);
    assert.match(line(/label text="Name"/), /^\s+\[\d+\] label text="Name" @\d+,\d+ \d+x\d+$/);
    assert.match(lines[0], /^\[\d+\] document "Order form"/);
  });

  test('coordinates are viewport pixels: named widgets carry none, elements below the fold are offscreen', () => {
    const list = elements.find((e) => e.role === 'list');
    const scrolled = { documents: [{ ...FIXTURE.snapshot.documents[0], scrollOffsetY: 5000 }] };
    const after = render(scrolled).elements.find((e) => e.id === list.id);
    assert.equal(after.rect[1], list.rect[1] - 5000);
    assert.equal(after.offscreen, true);
    assert.equal(list.offscreen, undefined);
    const send = elements.find((e) => e.name === 'Send order');
    assert.ok(Array.isArray(send.rect), 'the element keeps its rect for find()');
    assert.doesNotMatch(line(/button "Send order"/), /@/);
  });

  test('a large observation is capped with a note on how to reach the rest', () => {
    const out = capLines(lines, 600);
    assert.ok(out.length < 800);
    assert.match(out, /# … \d+ more elements not shown \(\d+ in all\): getAXState\(\{query/);
    assert.equal(capLines(lines), lines.join('\n'));
  });
});

describe('browser input validation', () => {
  test('keys map to DOM key events; editing chords carry their editor command', () => {
    assert.deepEqual(parseKey('ctrl+a'), { key: 'a', code: 'KeyA', windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65, modifiers: 2, commands: ['selectAll'] });
    assert.equal(parseKey('Return').text, '\r');
    assert.equal(parseKey('shift+Tab').modifiers, 8);
    assert.equal(parseKey('shift+a').key, 'A');
    assert.equal(parseKey('F5').windowsVirtualKeyCode, 116);
    assert.equal(parseKey('ctrl++').key, '+');
    assert.throws(() => parseKey('hyper+a'), /unknown modifier/);
    assert.throws(() => parseKey('Frobnicate'), /unknown key/);
  });

  test('navigation only to http(s) and file URLs', () => {
    assert.equal(checkUrl('https://example.org/a b'.replace(' ', '%20')), 'https://example.org/a%20b');
    assert.equal(checkUrl('file:///tmp/x.html'), 'file:///tmp/x.html');
    for (const bad of ['javascript:alert(1)', 'data:text/html,hi', 'about:blank', 'chrome://settings', '/relative', 'http://', 'https://x.org/a b']) {
      assert.throws(() => checkUrl(bad), /invalid_params|goto:/, bad);
    }
  });

  test('only a loopback debugging endpoint is accepted', () => {
    assert.deepEqual(parseEndpoint('http://127.0.0.1:9333'), { base: 'http://127.0.0.1:9333', host: '127.0.0.1:9333' });
    for (const bad of ['http://10.0.0.2:9222', 'http://example.org:9222', 'ws://127.0.0.1:9222', 'http://127.0.0.1', 'http://127.0.0.1:9222/json', 'nonsense']) {
      assert.throws(() => parseEndpoint(bad), (e) => e.code === 'browser_unavailable', bad);
    }
    assert.throws(() => parseEndpoint(undefined), /CUA_BROWSER_CDP/);
  });
});

// A browser double serving the recorded tree: records requests.
function recordedBrowser() {
  const { lines, elements } = render();
  const calls = [];
  return {
    calls,
    async call(method, params, { emit }) {
      calls.push({ method, params, emit });
      const tab = { url: 'http://127.0.0.1/', title: 'Order form' };
      if (method === 'elements') return { value: { elements }, tab };
      if (method === 'observe' || method === 'open') return { value: method === 'open' ? null : lines.join('\n'), outputs: emit ? [`browser tab\n<<<UNTRUSTED browser nonce=x>>>\n${lines.join('\n')}\n<<<END_UNTRUSTED nonce=x>>>`] : [], observed: emit, tab };
      return { value: undefined, acted: true, tab };
    },
  };
}

describe('browser tab in the sandbox', () => {
  let r, browser;
  const ids = Object.fromEntries(render().elements.filter((e) => e.name).map((e) => [`${e.role} ${e.name}`, e.id]));
  before(() => {
    browser = recordedBrowser();
    r = new SandboxRunner({ host: NO_ENGINE, browser, memoryLimitMb: 32 });
  });
  after(() => r.close());

  test('locators resolve against a fresh element list to exactly one ID; the state follows the last action', async () => {
    browser.calls.length = 0;
    const res = await r.eval({ code: `const tab = await cua.getTab({emit: false});
      await tab.setValue({role: 'textbox', name: 'Name'}, 'Ada'); await tab.click({role: 'button', name: 'send order'});
      await tab.setValue({name: 'Color', value: 'Blue'}); await tab.click(17, {clickCount: 2}); await tab.pressKey(['ctrl', 'a']); [tab.url, tab.title]`, timeoutMs: 5000 });
    assert.equal(res.ok, true, res.error);
    const acts = browser.calls.filter((c) => !['elements', 'open', 'observe'].includes(c.method));
    assert.deepEqual(acts.map((c) => [c.method, c.params]), [
      ['set_value', { element_index: ids['textbox Name'], value: 'Ada' }],
      ['click', { element_index: ids['button Send order'] }],
      ['set_value', { element_index: ids['combobox Color'], value: 'Blue' }],
      ['click', { element_index: 17, click_count: 2 }],
      ['press_key', { key: 'ctrl+a' }],
    ]);
    assert.equal(browser.calls.at(-1).method, 'observe', 'the end of the cell observes the tab');
    assert.match(res.outputs.at(-1).text, /^browser state after the last action: browser tab\n<<<UNTRUSTED browser/);
    assert.match(res.value, /"http:\/\/127.0.0.1\/",\s*"Order form"/);
  });

  test('ambiguous and missing locators fail with the candidates; find returns matches without showing them', async () => {
    let res = await r.eval({ code: 'try { await tab.click({role: "textbox"}) } catch (e) { e.code + ": " + e.message }', timeoutMs: 5000 });
    assert.match(res.value, /^ambiguous_element: click: 3 elements match .*textbox "Name".*textbox "Note".*textbox "Code"/);
    res = await r.eval({ code: 'try { await tab.click({name: "Checkout"}) } catch (e) { e.code + ": " + e.message }', timeoutMs: 5000 });
    assert.match(res.value, /^element_not_found: click: no element matches \{ ?"name": ?"Checkout" ?\} on the page/);
    res = await r.eval({ code: '(await tab.find({role: "link", text: /open item 1\\d/})).map((e) => e.name).join()', timeoutMs: 5000 });
    assert.equal(res.value, 'open item 10,open item 11,open item 12');
    assert.equal(res.outputs.length, 0);
  });

  test('waitFor text resolves from the elements; a getAXState after the last action replaces the end-of-cell state', async () => {
    browser.calls.length = 0;
    const res = await r.eval({ code: 'await tab.click({role: "button", name: "Save for later"}); const m = await tab.waitFor({text: "updated 12 days"}); await tab.getAXState(); m.length', timeoutMs: 5000 });
    assert.equal(res.ok, true, res.error);
    assert.equal(res.value, '1');
    assert.equal(browser.calls.filter((c) => c.method === 'observe').length, 1);
    assert.equal(res.outputs.length, 1);
  });

  test('without a configured browser, getBrowser fails clearly', async () => {
    const plain = new SandboxRunner({ host: NO_ENGINE, browser: new BrowserHost({ endpoint: undefined }), memoryLimitMb: 32 });
    try {
      const res = await plain.eval({ code: 'try { await cua.getBrowser() } catch (e) { e.code + ": " + e.message }', timeoutMs: 5000 });
      assert.match(res.value, /^browser_unavailable: no browser is configured/);
    } finally {
      await plain.close();
    }
  });
});

// Opt-in: CUA_BROWSER_CDP=http://127.0.0.1:PORT of a disposable browser
// (it navigates the browser's first tab).
describe('real browser over CDP', { skip: !process.env.CUA_BROWSER_CDP && 'set CUA_BROWSER_CDP to run' }, () => {
  let r, browser, site;
  before(async () => {
    site = await startServer();
    browser = new BrowserHost({ endpoint: process.env.CUA_BROWSER_CDP });
    r = new SandboxRunner({ host: NO_ENGINE, browser, memoryLimitMb: 64 });
  });
  after(async () => {
    await r.close();
    browser.close();
    site.server.close();
  });
  const run = async (code) => {
    const res = await r.eval({ code, timeoutMs: 30_000 });
    assert.equal(res.ok, true, `${res.error}\n${res.outputs.map((o) => o.text).join('\n')}`);
    return res;
  };

  test('fill a form, submit, wait for the result URL and read the result', async (t) => {
    await run(`const tab = await cua.getBrowser({emit: false}); await tab.goto(${JSON.stringify(site.url)})`);
    let res = await run(`const ms = (t) => Date.now() - t;
      let t = Date.now(); await tab.getAXState({emit: false}); const tObs = ms(t);
      t = Date.now(); const [note] = await tab.find({role: 'textbox', name: 'Note'}); const tFind = ms(t);
      t = Date.now(); await tab.setValue(note.id, 'ring twice'); const tSetId = ms(t);
      t = Date.now(); await tab.setValue({role: 'textbox', name: 'Name'}, 'Ada Lovelace'); const tSetLoc = ms(t);
      await tab.setValue({role: 'combobox', name: 'Color'}, 'Green');
      const [gift] = await tab.find({role: 'checkbox'});
      t = Date.now(); await tab.click(gift.id); const tClickId = ms(t);
      t = Date.now(); await tab.click({role: 'checkbox', name: 'Gift wrap'}); await tab.click({role: 'checkbox', name: 'Gift wrap'}); const tClickLoc = ms(t) / 2;
      [tObs, tFind, tSetId, tSetLoc, tClickId, tClickLoc].join()`);
    const [obs, find, setId, setLoc, clickId, clickLoc] = String(res.value).split(',').map(Number);
    t.diagnostic(`300-element page: getAXState ${obs} ms, find ${find} ms, setValue by ID ${setId} ms / by locator ${setLoc} ms, click by ID ${clickId} ms / by locator ${clickLoc} ms`);
    assert.match(res.outputs.at(-1).text, /checkbox "Gift wrap" \((focused, )?checked\)/);
    assert.match(res.outputs.at(-1).text, /textbox "Name" \(editable\) text="Ada Lovelace"/);
    res = await run(`await tab.click({role: 'button', name: 'Send order'}); await tab.waitFor({url: '/result'});
      (await tab.read({role: 'paragraph'}, {emit: false})).text`);
    assert.equal(res.value, 'Color g, gift yes, note ring twice');
    assert.match(res.outputs.at(-1).text, /heading "Thank you, Ada Lovelace"/);
  });

  test('below the fold is scrolled into view; a covered element and a rewritten value are refused', async () => {
    await run(`await tab.goto(${JSON.stringify(site.url)})`);
    let res = await run(`try { await tab.click({role: 'button', name: 'Covered'}) } catch (e) { e.code }`);
    assert.equal(res.value, 'element_covered');
    res = await run(`try { await tab.setValue({role: 'textbox', name: 'Code'}, 'abc') } catch (e) { e.code }`);
    assert.equal(res.value, 'value_mismatch');
    res = await run(`try { await tab.setValue({role: 'button', name: 'Send order'}, 'x') } catch (e) { e.code }`);
    assert.equal(res.value, 'not_editable');
    res = await run(`await tab.click({role: 'link', name: 'open item 100'}); tab.url`);
    assert.match(String(res.value), /\/item\/100"?$/);
  });
});

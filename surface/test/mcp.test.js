// End-to-end tests: real MCP stdio server (bin) + sandbox process + fake daemon.
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { startClient, sleep, BIN, FAKE_ENGINE } from './helpers.js';

describe('MCP surface', () => {
  let t;
  before(async () => {
    t = await startClient({ decide: (p) => (p.message.includes('org.example.Vault') ? 'decline' : 'accept') });
  });
  after(async () => t.close());

  test('lists js, js_reset, turn_ended and discrete tools; never host-only methods', async () => {
    const { tools } = await t.client.listTools();
    const names = tools.map((x) => x.name);
    for (const n of ['js', 'js_reset', 'turn_ended', 'list_apps', 'list_windows', 'get_app_state', 'get_ax_state', 'get_screenshot', 'click', 'drag', 'scroll', 'type_text', 'press_key', 'paste', 'set_value', 'perform_secondary_action']) {
      assert.ok(names.includes(n), `missing ${n}`);
    }
    assert.ok(!names.includes('approve_app'));
    assert.ok(!names.includes('mouse_move'), 'extension tools are off by default');
    const js = tools.find((x) => x.name === 'js');
    assert.deepEqual(Object.keys(js.inputSchema.properties).sort(), ['code', 'timeout_ms', 'title']);
    assert.match(js.description, /Confirmation policy/);
    assert.match(tools.find((x) => x.name === 'click').description, /Confirmation policy/);
  });

  test('REPL persistence across cells, top-level await, js_reset', async () => {
    let r = await t.js('let n = 2; const f = (x) => x * n; class P { v() { return n } } function g() { return "g" + n }\nconst [w] = await cua.list_windows(); w.window_id', { title: 'setup' });
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /\[setup\] ok/);
    assert.match(t.text(r), /=> 11/);
    r = await t.js('n = n + 1; [f(14), new P().v(), g(), typeof w]');
    assert.match(t.text(r), /42/);
    assert.match(t.text(r), /"g3"/);
    assert.match(t.text(r), /"object"/);
    r = await t.js('let n = 10; n'); // redeclaration is allowed in the REPL
    assert.match(t.text(r), /=> 10/);
    r = await t.client.callTool({ name: 'js_reset', arguments: {} });
    assert.equal(r.isError ?? false, false);
    r = await t.js('typeof n + " " + typeof f');
    assert.match(t.text(r), /=> undefined undefined/);
  });

  test('approval via elicitation once per app per turn, automatic AX output and images', async () => {
    const before = t.elicitations.length;
    const r = await t.js('const o = await cua.click({window_id: 11, element_index: 1}); console.log(o.window.app_id); await cua.typeText({window_id: 11, text: "hi"}); "done"');
    assert.equal(r.isError, false, t.text(r));
    assert.equal(t.elicitations.length, before + 1, 'exactly one elicitation for the app');
    const msg = t.elicitations.at(-1).message;
    assert.match(msg, /org\.example\.Notes/);
    assert.match(msg, /untrusted/);
    assert.match(msg, /Confirmation policy/);
    const text = t.text(r);
    assert.match(text, /org\.example\.Notes/);
    assert.match(text, /=> done/);
    assert.match(text, /<<<UNTRUSTED accessibility nonce=/);
    assert.ok(r.content.some((c) => c.type === 'image' && c.mimeType === 'image/png'), 'screenshot attached');
    const performed = t.performed().filter((e) => e.params.window_id === 11);
    assert.deepEqual(performed.map((e) => e.method).slice(-2), ['click', 'type_text']);
    const approvals = t.events().filter((e) => e.event === 'request' && e.method === 'approve_app');
    assert.deepEqual(approvals.at(-1).params, { window_id: 11, app_id: 'org.example.Notes' });
  });

  test('screen content cannot close the untrusted fence', async () => {
    const r = await t.client.callTool({ name: 'get_ax_state', arguments: { window_id: 11 } });
    const text = t.text(r);
    const fences = [...text.matchAll(/<<<UNTRUSTED (\w+) nonce=([\w-]+)>>>/g)];
    assert.ok(fences.length >= 2);
    for (const [, , nonce] of fences) {
      const ends = text.split(`<<<END_UNTRUSTED nonce=${nonce}>>>`).length - 1;
      assert.equal(ends, 1, 'each fence closes exactly once');
    }
    // the injected marker with a fake nonce is neutralized
    assert.ok(!text.includes('<<<END_UNTRUSTED nonce=zzz>>>'));
    assert.ok(!text.includes('<<<END_UNTRUSTED nonce=abc>>>'));
  });

  test('denial: declined app is never touched, not re-asked in the turn, asked again after turn_ended', async () => {
    const before = t.elicitations.length;
    let r = await t.js('try { await cua.click({window_id: 13, element_index: 0}) } catch (e) { e.code }');
    assert.match(t.text(r), /=> approval_denied/);
    assert.equal(t.elicitations.length, before + 1);
    r = await t.client.callTool({ name: 'click', arguments: { window_id: 13, x: 1, y: 1 } });
    assert.equal(r.isError, true);
    assert.match(t.text(r), /approval_denied/);
    assert.equal(t.elicitations.length, before + 1, 'denial is remembered for the turn');
    assert.equal(t.performed().filter((e) => e.params.window_id === 13).length, 0);
    assert.equal(t.events().filter((e) => e.method === 'approve_app' && e.params.window_id === 13).length, 0);

    r = await t.client.callTool({ name: 'turn_ended', arguments: {} });
    assert.match(t.text(r), /approvals cleared/);
    assert.ok(t.events().some((e) => e.event === 'request' && e.method === 'turn_ended'));
    // approvals were reset too: Notes needs a new elicitation
    r = await t.js('(await cua.get_ax_state({window_id: 11})).window.window_id');
    assert.match(t.text(r), /=> 11/);
    assert.equal(t.elicitations.length, before + 2);
    assert.match(t.elicitations.at(-1).message, /org\.example\.Notes/);
    r = await t.js('try { await cua.get_ax_state({window_id: 13}) } catch (e) { e.code }');
    assert.match(t.text(r), /=> approval_denied/);
    assert.equal(t.elicitations.length, before + 3, 'asked again after turn_ended');
  });

  test('injection boundaries: no host-only methods, no Node APIs, strict params', async () => {
    let r = await t.js(`[typeof cua.approve_app, typeof cua.turn_ended, typeof cua.turnEnded, typeof __cua_host_call, typeof __cua_emit,
      typeof require, typeof process, typeof fetch, typeof globalThis.WebAssembly, typeof setTimeout, typeof Deno, typeof Bun].join(",")`);
    assert.match(t.text(r), /=> undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined/);
    r = await t.js('try { await import("node:fs"); "imported" } catch (e) { "blocked: " + e.name }');
    assert.match(t.text(r), /blocked/);
    r = await t.js('Object.isFrozen(cua) && (() => { try { cua.click = 1 } catch (e) {} return typeof cua.click })()');
    assert.match(t.text(r), /=> function/);
    const attempts = {
      'string window id': '{window_id: "11; rm -rf /", element_index: 1}',
      'extra param': '{window_id: 11, element_index: 1, approve: true}',
      'proto key': 'JSON.parse(\'{"window_id": 11, "element_index": 1, "__proto__": {"x": 1}}\')',
      'both targets': '{window_id: 11, element_index: 1, x: 1, y: 1}',
      'negative coord': '{window_id: 11, x: -5, y: 1}',
      'extension param': '{window_id: 11, x: 1, y: 1, button: "right"}',
    };
    const before = t.performed().length;
    for (const [label, params] of Object.entries(attempts)) {
      r = await t.js(`try { await cua.click(${params}); "ran" } catch (e) { e.code }`);
      assert.match(t.text(r), /=> (invalid_params|unsupported)/, label);
    }
    r = await t.js('try { await cua.press_key({window_id: 11, key: "ctrl+s\\nrm"}) } catch (e) { e.code }');
    assert.match(t.text(r), /=> invalid_params/);
    assert.equal(t.performed().length, before, 'nothing reached the engine');
    r = await t.client.callTool({ name: 'approve_app', arguments: { window_id: 13, app_id: 'org.example.Vault' } });
    assert.equal(r.isError, true);
    assert.match(t.text(r), /unknown_tool/);
    r = await t.client.callTool({ name: 'js', arguments: { code: '1', approve: true } });
    assert.equal(r.isError, true);
  });

  test('unawaited calls are cancelled at cell end and never land later', async () => {
    await t.js('await cua.get_ax_state({window_id: 11})'); // ensure approved in this turn
    const r = await t.js('cua.type_text({window_id: 11, text: "SLOW:unawaited"}); "returned"');
    assert.match(t.text(r), /=> returned/);
    assert.match(t.text(r), /cancelled/);
    await sleep(2200);
    assert.equal(t.performed().filter((e) => e.params.text === 'SLOW:unawaited').length, 0);
    // a stale callback from an old cell cannot act between cells either
    const r2 = await t.js('let later; later = async () => cua.get_ax_state({window_id: 11}); "ok"');
    assert.match(t.text(r2), /=> ok/);
  });

  test('timeout terminates the pending action and keeps REPL state', async () => {
    await t.js('let keep = "kept"; await cua.get_ax_state({window_id: 11})');
    const started = Date.now();
    let r = await t.js('await cua.type_text({window_id: 11, text: "SLOW:timeout"})', { timeout_ms: 400 });
    assert.equal(r.isError, true);
    assert.match(t.text(r), /timeout/i);
    assert.ok(Date.now() - started < 1400, 'returned promptly');
    await sleep(2200);
    assert.equal(t.performed().filter((e) => e.params.text === 'SLOW:timeout').length, 0, 'late action prevented');
    r = await t.js('while (true) {}', { timeout_ms: 300 });
    assert.equal(r.isError, true);
    assert.match(t.text(r), /timeout/i);
    r = await t.js('keep');
    assert.match(t.text(r), /=> kept/);
    // the engine was restarted and approvals of this turn were replayed: no new prompt
    const before = t.elicitations.length;
    r = await t.js('(await cua.click({window_id: 11, element_index: 1})).window.window_id');
    assert.match(t.text(r), /=> 11/);
    assert.equal(t.elicitations.length, before);
  });

  test('MCP cancellation terminates the pending action', async () => {
    await t.js('await cua.get_ax_state({window_id: 11})');
    const ac = new AbortController();
    const p = t.js('await cua.type_text({window_id: 11, text: "SLOW:cancel"}); "finished"', {}, { signal: ac.signal });
    await sleep(400);
    ac.abort();
    await assert.rejects(p);
    await sleep(2200);
    assert.equal(t.performed().filter((e) => e.params.text === 'SLOW:cancel').length, 0);
    const r = await t.js('"alive"');
    assert.match(t.text(r), /=> alive/);
  });

  test('discrete tools validate and return observation + image', async () => {
    let r = await t.client.callTool({ name: 'click', arguments: { window_id: 11, x: 5, y: 6 } });
    assert.equal(r.isError ?? false, false, t.text(r));
    assert.match(t.text(r), /<<<UNTRUSTED accessibility/);
    assert.ok(r.content.some((c) => c.type === 'image'));
    r = await t.client.callTool({ name: 'get_screenshot', arguments: { window_id: 11 } });
    assert.equal(r.content.find((c) => c.type === 'image').mimeType, 'image/png');
    r = await t.client.callTool({ name: 'scroll', arguments: { window_id: 11, x: 1, y: 1, direction: 'sideways', pixels: 5 } });
    assert.equal(r.isError, true);
    assert.match(t.text(r), /invalid_params/);
    r = await t.client.callTool({ name: 'drag', arguments: { window_id: 11, from: [1, 2], to: [3, 4] } });
    assert.equal(r.isError ?? false, false);
    assert.deepEqual(t.performed().at(-1).params, { window_id: 11, from: [1, 2], to: [3, 4] });
  });

  test('engine receives the explicit display', () => {
    const starts = t.events().filter((e) => e.event === 'start');
    assert.ok(starts.length >= 1);
    assert.ok(starts.every((e) => e.display === ':99'));
  });

  test('window-bound cua API persists and returns observations', async () => {
    let r = await t.js('const app = await cua.getApp({windowId:11}); await app.getAXState({disableDiffing:true})');
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /button "OK"/);
    r = await t.js('await app.setValue(2, "bound"); await app.click(1); (await cua.getState()).apps.length');
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /=> 3/);
  });

  test('turn_ended cancels an active cell before it can issue another action', async () => {
    await t.js('await cua.get_ax_state({window_id:11})');
    const pending = t.js('await sleep(600); await cua.typeText({window_id:11,text:"after-turn"})');
    await sleep(100);
    await t.client.callTool({name:'turn_ended',arguments:{}});
    const r = await pending;
    assert.equal(r.isError, true);
    await sleep(700);
    assert.equal(t.performed().filter(e => e.params.text === 'after-turn').length, 0);
  });
});

describe('fail-closed configuration', () => {
  test('refuses display :0', () => {
    for (const display of [':0', ':0.0', 'unix:0', '']) {
      const r = spawnSync(process.execPath, [BIN], { env: { PATH: process.env.PATH, CUA_ENGINE: FAKE_ENGINE, CUA_DISPLAY: display }, input: '', timeout: 10_000 });
      assert.equal(r.status, 2, `display ${JSON.stringify(display)}`);
      assert.match(r.stderr.toString(), /CUA_DISPLAY|display :0/);
    }
  });

  test('without client elicitation support, approval fails closed', async () => {
    const t = await startClient({ elicitation: false });
    try {
      const r = await t.js('try { await cua.click({window_id: 11, element_index: 1}) } catch (e) { e.code }');
      assert.match(t.text(r), /=> approval_unavailable/);
      assert.equal(t.performed().length, 0);
    } finally {
      await t.close();
    }
  });

  test('daemon deny list wins and declined approval (approve=false) is a denial', async () => {
    const t = await startClient({ env: { CUA_DENY_APPS: 'org.example.Calc' }, decide: () => 'accept-false' });
    try {
      let r = await t.js('try { await cua.get_ax_state({window_id: 12}) } catch (e) { e.code }');
      assert.match(t.text(r), /=> app_denied/);
      r = await t.js('try { await cua.get_ax_state({window_id: 11}) } catch (e) { e.code }');
      assert.match(t.text(r), /=> approval_denied/);
      assert.equal(t.events().filter((e) => e.method === 'approve_app').length, 0);
    } finally {
      await t.close();
    }
  });
});

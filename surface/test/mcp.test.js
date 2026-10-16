// End-to-end tests: real MCP stdio server (bin) + sandbox process + fake daemon.
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { startClient, sleep, BIN, FAKE_ENGINE } from './helpers.js';
import { HostApi } from '../src/host.js';

// Minimal engine double for HostApi: windows needing approval until approved.
function stubEngine(windows) {
  const approved = new Set();
  return {
    approved,
    async request(method, p) {
      if (method === 'approve_app') { approved.add(p.window_id); return { approved: true }; }
      const w = windows.find((x) => x.window_id === p.window_id);
      if (!approved.has(w.window_id)) {
        throw Object.assign(new Error('approval required'), { code: 'approval_required', data: { ...w } });
      }
      return { window: w, accessibility: '', warnings: [] };
    },
  };
}

const APP = 'const app = await cua.getApp({windowId: 11}, {emit: false});';

describe('MCP surface', () => {
  let t;
  before(async () => {
    t = await startClient({ decide: (p) => (p.message.includes('org.example.Vault') ? 'decline' : 'accept') });
  });
  after(async () => t.close());

  test('lists js, js_reset, turn_ended and discrete tools; never host-only methods', async () => {
    const { tools } = await t.client.listTools();
    const names = tools.map((x) => x.name);
    for (const n of ['js', 'js_reset', 'turn_ended', 'list_apps', 'list_windows', 'get_app_state', 'get_ax_state', 'get_screenshot', 'launch_app', 'clipboard_write', 'clipboard_read', 'clipboard_release', 'click', 'drag', 'scroll', 'type_text', 'press_key', 'paste', 'set_value', 'perform_secondary_action', 'mouse_move']) {
      assert.ok(names.includes(n), `missing ${n}`);
    }
    assert.ok(!names.includes('approve_app'));
    const click = tools.find((x) => x.name === 'click').inputSchema.properties;
    assert.deepEqual(Object.keys(click).sort(), ['app', 'button', 'click_count', 'element_index', 'hold_ms', 'modifiers', 'screenshot', 'window_id', 'x', 'y']);
    assert.equal(tools.find((x) => x.name === 'launch_app').inputSchema.properties.app.type, 'string');
    const js = tools.find((x) => x.name === 'js');
    assert.deepEqual(Object.keys(js.inputSchema.properties).sort(), ['code', 'timeout_ms', 'title']);
    assert.match(js.description, /Confirmation policy/);
    assert.match(js.description, /nodeRepl\.emitImage/);
    assert.match(tools.find((x) => x.name === 'click').description, /Confirmation policy/);
  });

  test('REPL persistence across cells, top-level await, js_reset', async () => {
    let r = await t.js('let n = 2; const f = (x) => x * n; class P { v() { return n } } function g() { return "g" + n }\nconst [w] = await cua.listWindows({emit: false}); w.id', { title: 'setup' });
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /\[setup\] ok/);
    assert.match(t.text(r), /=> 14/);
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

  test('OpenAI-compatible discovery shapes; window_id stays on the raw side', async () => {
    let r = await t.js(`const ws = await cua.listWindows({emit: false});
      const apps = await cua.listApps({emit: false});
      const state = await cua.getState({emit: false});
      const calc = await cua.getApp({appId: 'org.example.Calc'}, {emit: false});
      JSON.stringify([ws.map((w) => Object.keys(w).sort().join('|')), Object.keys(apps[0]).sort(), Object.keys(state).sort(),
        apps[0].windows[0].id, calc.id, calc.windowId, calc.windows[0].app])`);
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /=> \[\["app\|id\|title","app\|id\|title","app\|id\|title","app\|id\|title","app\|id\|title","app\|id\|title"\],\["displayName","id","isRunning","windows"\],\["apps","browsers"\],14,"org.example.Calc",12,"org.example.Calc"\]/);
    assert.doesNotMatch(t.text(r), /window_id/);
    // Discovery calls emit their (untrusted) result by default.
    r = await t.js('await cua.listWindows(); await cua.getApp({windowId: 12}); "x"');
    assert.match(t.text(r), /<<<UNTRUSTED windows nonce=[^>]*>>>[^]*"id":11/);
    assert.match(t.text(r), /<<<UNTRUSTED app nonce=[^>]*>>>[^]*"displayName":"org.example.Calc"/);
    r = await t.js('try { await cua.getApp({windowId: 999}) } catch (e) { e.code }');
    assert.match(t.text(r), /=> window_not_found/);
  });

  test('approval via elicitation once per app per turn; final state after the last action', async () => {
    const before = t.elicitations.length;
    const r = await t.js(`${APP} await app.click(1); await app.typeText("hi"); "done"`);
    assert.equal(r.isError, false, t.text(r));
    assert.equal(t.elicitations.length, before + 1, 'exactly one elicitation for the app');
    const req = t.elicitations.at(-1);
    assert.match(req.message, /org\.example\.Notes/);
    assert.match(req.message, /untrusted/);
    assert.match(req.message, /Confirmation policy/);
    assert.deepEqual(req.requestedSchema.properties.decision.enum, ['allow_turn', 'allow_session', 'allow_always', 'deny']);
    const text = t.text(r);
    assert.match(text, /=> done/);
    assert.match(text, /state after the last action[^]*<<<UNTRUSTED accessibility nonce=/);
    // The end-of-cell state is a fresh observation (the actions ran with
    // observe: false), which carries no screenshot.
    assert.ok(!r.content.some((c) => c.type === 'image'), 'no image unless the cell asked for one');
    const performed = t.performed().filter((e) => e.params.window_id === 11);
    assert.deepEqual(performed.map((e) => e.method).slice(-2), ['click', 'type_text']);
    const approvals = t.events().filter((e) => e.event === 'request' && e.method === 'approve_app');
    assert.deepEqual(approvals.at(-1).params, { window_id: 11, app_id: 'org.example.Notes' });
  });

  test('getApp by name picks the main window; app methods follow dialogs; GPT argument shapes', async () => {
    let r = await t.js(`const notes = await cua.getApp("org.example.notes", {emit: false});
      await notes.click({element: 1}); await notes.click({x: 3, y: 4}); await notes.typeText({text: "x"});
      await notes.pressKey({key: "Ctrl+s"}); await notes.pressKey(["ctrl", "shift", "n"]);
      await notes.setValue({element: 3, value: 5}); await notes.scroll({x: 1, y: 2, scrollY: -300});
      JSON.stringify([notes.windowId, notes.windows.map((w) => w.id).sort()])`);
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /=> \[11,\[11,14\]\]/, 'main window, not the newer dialog; the app lists both');
    // Actions in a cell skip the post-action tree (observe: false); the
    // cell's end observes once.
    const done = t.performed().slice(-7).map((e) => [e.method, e.params]);
    assert.deepEqual(done, [
      ['click', { window_id: 11, app: true, element_index: 1, observe: false }],
      ['click', { window_id: 11, app: true, x: 3, y: 4, frame_window: 11, observe: false }],
      ['type_text', { window_id: 11, app: true, text: 'x', frame_window: 11, observe: false }],
      ['press_key', { window_id: 11, app: true, key: 'Ctrl+s', observe: false }],
      ['press_key', { window_id: 11, app: true, key: 'ctrl+shift+n', observe: false }],
      ['set_value', { window_id: 11, app: true, element_index: 3, value: '5', observe: false }],
      ['scroll', { window_id: 11, app: true, x: 1, y: 2, direction: 'up', pixels: 300, frame_window: 11, observe: false }],
    ]);
    // Locators: role spellings, exact names before partial ones, regexes,
    // text across name/description/value, the object forms.
    r = await t.js(`const loc = await cua.getApp({windowId: 11}, {emit: false});
      await loc.click({role: "push button", name: "ok"}); await loc.setValue({role: "text", name: /^ignore/i}, "v");
      await loc.setValue({text: "previous instructions", value: "w"}); await loc.performSecondaryAction({role: "button", name: "OK", action: "activate"});
      JSON.stringify((await loc.find({role: "textbox"})).map((e) => [e.id, e.role]))`);
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /=> \[\[2,"text"\],\[3,"entry"\]\]/);
    assert.deepEqual(t.performed().slice(-4).map((e) => [e.method, e.params.element_index, e.params.frame_window]), [
      ['click', 1, undefined], ['set_value', 2, undefined], ['set_value', 2, undefined], ['perform_secondary_action', 1, undefined],
    ]);
    // No match or several matches: nothing reaches the engine; the error lists the candidates.
    const before = t.performed().length;
    r = await t.js('await loc.click({role: "button", name: "Cancel"})');
    assert.match(t.text(r), /no element matches \{ "role": "button", "name": "Cancel" \}.*\[element_not_found\]/s);
    r = await t.js('await loc.click({role: "text"})');
    assert.match(t.text(r), /2 elements match .*\[2\] text .*\[3\] entry.*\[ambiguous_element\]/s);
    assert.equal(t.performed().length, before);
    // A returned observation that was emitted is not repeated as the value.
    r = await t.js(`const n2 = await cua.getApp({windowId: 11}, {emit: false}); await n2.getAXState({disableDiffing: true})`);
    assert.equal(t.text(r).split('[1] button "OK"').length - 1, 1, t.text(r));
  });

  test('continuous drags preserve every point; invalid paths send no input', async () => {
    const r = await t.js(`${APP} await app.drag({path: [[10,20], {x:30,y:20}, [30,40], [10,20]]});`);
    assert.equal(r.isError, false, t.text(r));
    assert.deepEqual(t.performed().at(-1).params.path, [[10,20], [30,20], [30,40], [10,20]]);
    for (const code of [
      'await app.drag({path: [[1,2]]})',
      'await app.drag({path: [[1,2], [3.5,4]]})',
      'await app.drag({path: [[1,2], [3,4]], from: [1,2]})',
      'await app.drag({path: Array.from({length: 257}, () => [1,2])})',
    ]) {
      const before = t.performed().length;
      const bad = await t.js(code);
      assert.equal(bad.isError, true, t.text(bad));
      assert.match(t.text(bad), /invalid_params/);
      assert.equal(t.performed().length, before);
    }
  });

  test('held modifiers, press durations, hover and mouse buttons reach the engine', async () => {
    const r = await t.js(`${APP} await app.click([5, 6], {modifiers: "shift+ctrl", durationMs: 50}); await app.rightClick(1); await app.doubleClick([2, 2], {key: "alt"});
      await app.pressKey("a", {durationMs: 20}); await app.moveMouse([7, 8]); await app.moveMouseRelative(1, -1); await app.hover(1);
      await app.drag([1, 1], [2, 2], {modifiers: ["alt"]}); await app.drag({path: [[1, 1], [2, 2]], modifiers: "super"});
      await app.scroll({x: 3, y: 3, direction: "down", modifiers: ["shift"]}); await app.scroll([3, 3], "up", 1, {modifiers: "ctrl"}); "done"`);
    assert.equal(r.isError, false, t.text(r));
    const strip = ({ window_id, app, observe, frame_window, ...rest }) => rest;
    assert.deepEqual(t.performed().slice(-11).map((e) => [e.method, strip(e.params)]), [
      ['click', { x: 5, y: 6, modifiers: ['shift', 'ctrl'], hold_ms: 50 }],
      ['click', { element_index: 1, button: 'right' }],
      ['click', { x: 2, y: 2, click_count: 2, modifiers: ['alt'] }],
      ['press_key', { key: 'a', hold_ms: 20 }],
      ['mouse_move', { x: 7, y: 8 }],
      ['mouse_move', { dx: 1, dy: -1 }],
      ['mouse_move', { element_index: 1 }],
      ['drag', { from: [1, 1], to: [2, 2], modifiers: ['alt'] }],
      ['drag', { path: [[1, 1], [2, 2]], modifiers: ['super'] }],
      ['scroll', { x: 3, y: 3, direction: 'down', pixels: 500, modifiers: ['shift'] }],
      ['scroll', { x: 3, y: 3, direction: 'up', pixels: 500, modifiers: ['ctrl'] }],
    ]);
  });

  test('getAXStateAndScreenshot is one engine call that returns both', async () => {
    const r = await t.js(`${APP} const {state, screenshot} = await app.getAXStateAndScreenshot(); [typeof state, screenshot instanceof Uint8Array && screenshot.length > 0].join()`);
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /=> string,true/);
    assert.match(t.text(r), /host calls: list_windows, get_ax_state$/m);
    assert.ok(r.content.some((c) => c.type === 'image'), 'the screenshot is shown');
    assert.match(t.text(r), /<<<UNTRUSTED accessibility/);
    assert.equal(t.events().filter((e) => e.event === 'request' && e.method === 'get_ax_state' && e.params.screenshot === true).length >= 1, true);
  });

  test('launchApp starts an installed app and returns it like getApp; clipboard round trip; help', async () => {
    let r = await t.js('const launched = await cua.launchApp("drawing"); launched.id');
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /launched Launched \(pid 121\)/);
    assert.match(t.text(r), /<<<UNTRUSTED accessibility/);
    assert.match(t.text(r), /=> org\.example\.Launched/);
    assert.deepEqual(t.performed().find((e) => e.method === 'launch_app').params, { app: 'drawing' });
    r = await t.js('try { await cua.launchApp("nothing") } catch (e) { e.code }');
    assert.match(t.text(r), /=> not_found/);
    r = await t.js('await cua.clipboard.write("Grüße λ"); const a = await cua.clipboard.read(); await cua.clipboard.release(); [a, await cua.clipboard.read()]');
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /"Grüße λ",\s*null/);
    r = await t.js('cua.help()');
    assert.match(t.text(r), /Run JavaScript in a persistent, isolated REPL/);
    assert.match(t.text(r), /Confirmation policy/);
  });

  test('getApp emits the app state once; prefers the focused main window and names the others', async () => {
    let r = await t.js(`const two = await cua.getApp("org.example.two"); await two.getAXState(); two.windowId`);
    assert.equal(r.isError, false, t.text(r));
    const text = t.text(r);
    assert.match(text, /=> 16/, 'the focused main window');
    assert.match(text, /other main windows of org\.example\.Two: <<<UNTRUSTED windows[^]*15 "Two A"/);
    assert.equal(text.split('[1] button "OK"').length - 1, 1, 'getApp showed the state; getAXState right after does not repeat it');
    r = await t.js(`const two2 = await cua.getApp("org.example.Two", {emit: false}); "q"`);
    assert.doesNotMatch(t.text(r), /UNTRUSTED accessibility/);
  });

  test('coordinates refer to the window the model last saw, not one an action opened unseen', async () => {
    const r = await t.js(`const n3 = await cua.getApp("org.example.Notes", {emit: false});
      await n3.getAXState({emit: false});
      await n3.click([777, 1]);   // opens the Save As dialog (14), unseen
      await n3.click([5, 5]); "x"`);
    assert.equal(r.isError, false, t.text(r));
    let clicks = t.performed().filter((e) => e.method === 'click');
    assert.equal(clicks.at(-1).params.frame_window, 11, 'second click refers to the window seen before the dialog');
    // The cell output showed the dialog; the next coordinates refer to it.
    assert.match(t.text(r), /state after the last action: window 14/);
    await t.js(`await n3.click([6, 6]); await n3.pressKey("Escape")`);
    clicks = t.performed().filter((e) => e.method === 'click');
    assert.equal(clicks.at(-1).params.frame_window, 14);
  });

  test('nodeRepl output API: write, emitImage, auto-emitted observations, PNG bytes', async () => {
    let r = await t.js(`${APP}
      nodeRepl.write({a: 1}); nodeRepl.write("plain text");
      const png = await app.getScreenshot({emit: false});
      await nodeRepl.emitImage(png);
      await nodeRepl.emitImage({bytes: png.slice(), mimeType: 'image/png'});
      [png instanceof Uint8Array, png[0], png[1], png[2], png[3]].join()`);
    assert.equal(r.isError, false, t.text(r));
    assert.match(t.text(r), /\{\n  "a": 1\n\}\nplain text/);
    assert.match(t.text(r), /=> true,137,80,78,71/);
    const images = r.content.filter((c) => c.type === 'image');
    assert.equal(images.length, 2);
    assert.equal(images[0].data, images[1].data, 're-encoded bytes round-trip exactly');
    r = await t.js(`${APP} await app.getScreenshot(); await app.getAXState({emit: false}); "x"`);
    assert.equal(r.content.filter((c) => c.type === 'image').length, 1, 'getScreenshot emits by default');
    assert.doesNotMatch(t.text(r), /UNTRUSTED accessibility/, 'emit:false suppresses the tree');
    // A screenshot as the cell value says whether it is already shown.
    r = await t.js(`${APP} await app.getScreenshot()`);
    assert.match(t.text(r), /=> Uint8Array\(\d+\) \[PNG image, shown in this output\]/);
    r = await t.js(`${APP} await app.getScreenshot({emit: false})`);
    assert.match(t.text(r), /\[PNG image; pass it to nodeRepl\.emitImage\(\) to show it\]/);
    r = await t.js(`${APP} await app.getAXStateAndScreenshot(); "y"`);
    assert.match(t.text(r), /<<<UNTRUSTED accessibility nonce=/);
    assert.equal(r.content.filter((c) => c.type === 'image').length, 1);
    r = await t.js('try { await nodeRepl.emitImage(new Uint8Array([1, 2, 3])) } catch (e) { e.code }');
    assert.match(t.text(r), /=> invalid_params/);
  });

  test('getAXState diffs are relative to the tree the model last received', async () => {
    let r = await t.js(`${APP} await app.getAXState({disableDiffing: true}); "baseline"`);
    const baseline = t.text(r).match(/\[3\] entry ("[^"\n]*")/)[1];
    // One cell: an internal (not emitted) observation between two actions.
    r = await t.js(`${APP}
      await app.typeText("a");
      await app.getAXState({emit: false});
      await app.typeText("b");
      await app.getAXState(); "end"`);
    assert.equal(r.isError, false, t.text(r));
    const text = t.text(r);
    const now = JSON.stringify(JSON.parse(baseline) + 'ab');
    assert.ok(text.includes(`~ [3] entry ${baseline}\n  -> entry ${now}`), text);
    assert.doesNotMatch(text, /state after the last action/, 'the emitted tree already showed the final state');
    // Without an emitted observation, the cell output carries the post-action state as a diff.
    r = await t.js(`${APP} await app.typeText("c")`);
    assert.ok(t.text(r).includes(`~ [3] entry ${now}\n  -> entry ${JSON.stringify(JSON.parse(now) + 'c')}`), t.text(r));
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
    let r = await t.js('try { const v = await cua.getApp({windowId: 13}, {emit: false}); await v.click(0) } catch (e) { e.code }');
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
    // turn approvals were reset too: Notes needs a new elicitation
    r = await t.js(`${APP} await app.getAXState({emit: false}); app.windowId`);
    assert.match(t.text(r), /=> 11/);
    assert.equal(t.elicitations.length, before + 2);
    assert.match(t.elicitations.at(-1).message, /org\.example\.Notes/);
    r = await t.js('try { await (await cua.getApp({windowId: 13}, {emit: false})).getAXState() } catch (e) { e.code }');
    assert.match(t.text(r), /=> approval_denied/);
    assert.equal(t.elicitations.length, before + 3, 'asked again after turn_ended');
  });

  test('injection boundaries: no raw or host-only methods, no Node APIs, strict params', async () => {
    let r = await t.js(`[typeof cua.approve_app, typeof cua.turn_ended, typeof cua.click, typeof cua.list_windows, typeof __cua_host_call, typeof __cua_emit,
      typeof require, typeof process, typeof fetch, typeof globalThis.WebAssembly, typeof setTimeout, typeof Deno, typeof Bun].join(",")`);
    assert.match(t.text(r), /=> undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined/);
    r = await t.js('try { await import("node:fs"); "imported" } catch (e) { "blocked: " + e.name }');
    assert.match(t.text(r), /blocked/);
    r = await t.js('Object.isFrozen(cua) && Object.isFrozen(nodeRepl) && (() => { try { cua.getApp = 1 } catch (e) {} return typeof cua.getApp })()');
    assert.match(t.text(r), /=> function/);
    const attempts = {
      'string target': 'app.click("11; rm -rf /")',
      'extra option': 'app.click(1, {approve: true})',
      'negative coord': 'app.click([-5, 1])',
      'unknown button': 'app.click([1, 1], {mouseButton: "back"})',
      'unknown modifier': 'app.click([1, 1], {modifiers: ["cmd"]})',
      'hold too long': 'app.click([1, 1], {durationMs: 99999})',
      'launch with a command line': 'cua.launchApp("xterm -e rm\\n")',
      'key injection': 'app.pressKey("ctrl+s\\nrm")',
      'non-string text': 'app.typeText({toString() { return "x" }})',
    };
    const before = t.performed().length;
    for (const [label, call] of Object.entries(attempts)) {
      r = await t.js(`${APP} try { await ${call}; "ran" } catch (e) { e.code }`);
      assert.match(t.text(r), /=> (invalid_params|unsupported)/, label);
    }
    assert.equal(t.performed().length, before, 'nothing reached the engine');
    r = await t.client.callTool({ name: 'approve_app', arguments: { window_id: 13, app_id: 'org.example.Vault' } });
    assert.equal(r.isError, true);
    assert.match(t.text(r), /unknown_tool/);
    r = await t.client.callTool({ name: 'js', arguments: { code: '1', approve: true } });
    assert.equal(r.isError, true);
  });

  test('unawaited calls are cancelled at cell end and never land later', async () => {
    await t.js(`${APP} await app.getAXState({emit: false})`); // ensure approved in this turn
    const r = await t.js(`${APP} app.typeText("SLOW:unawaited"); "returned"`);
    assert.match(t.text(r), /=> returned/);
    assert.match(t.text(r), /cancelled/);
    await sleep(2200);
    assert.equal(t.performed().filter((e) => e.params.text === 'SLOW:unawaited').length, 0);
    // a stale callback from an old cell cannot act between cells either
    const r2 = await t.js(`${APP} let later; later = async () => app.getAXState(); "ok"`);
    assert.match(t.text(r2), /=> ok/);
  });

  test('timeout terminates the pending action and keeps REPL state', async () => {
    await t.js(`let keep = "kept"; ${APP} await app.getAXState({emit: false})`);
    const started = Date.now();
    let r = await t.js(`${APP} await app.typeText("SLOW:timeout")`, { timeout_ms: 400 });
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
    r = await t.js(`${APP} await app.click(1); app.windowId`);
    assert.match(t.text(r), /=> 11/);
    assert.equal(t.elicitations.length, before);
    // The new daemon continues the session's element IDs: an old ID can
    // never name a different element (the fake's last ID is first + 3).
    const firstIds = t.events().filter((e) => e.event === 'start')
      .map((e) => Number(e.argv[e.argv.indexOf('--first-element-id') + 1]));
    assert.ok(firstIds.length >= 2, JSON.stringify(firstIds));
    assert.ok(firstIds.at(-1) > firstIds.at(-2) + 3, JSON.stringify(firstIds));
  });

  test('MCP cancellation terminates the pending action', async () => {
    await t.js(`${APP} await app.getAXState({emit: false})`);
    const ac = new AbortController();
    const p = t.js(`${APP} await app.typeText("SLOW:cancel"); "finished"`, {}, { signal: ac.signal });
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

  test('turn_ended cancels an active cell before it can issue another action', async () => {
    await t.js(`${APP} await app.getAXState({emit: false})`);
    const pending = t.js(`${APP} await sleep(600); await app.typeText("after-turn")`);
    await sleep(100);
    await t.client.callTool({ name: 'turn_ended', arguments: {} });
    const r = await pending;
    assert.equal(r.isError, true);
    await sleep(700);
    assert.equal(t.performed().filter((e) => e.params.text === 'after-turn').length, 0);
  });
});

describe('approval scopes', () => {
  test('session approval survives turn_ended without a new prompt', async () => {
    const t = await startClient({ decide: () => 'session' });
    try {
      let r = await t.js(`${APP} await app.getAXState({emit: false}); "a"`);
      assert.match(t.text(r), /=> a/);
      await t.client.callTool({ name: 'turn_ended', arguments: {} });
      r = await t.js(`${APP} await app.click(1); "b"`);
      assert.match(t.text(r), /=> b/);
      assert.equal(t.elicitations.length, 1);
      // The daemon forgot its approval at turn end; the host re-granted it.
      assert.equal(t.events().filter((e) => e.method === 'approve_app').length, 2);
    } finally {
      await t.close();
    }
  });

  test('always approval persists in the operator file; deny lists still win', async () => {
    const t = await startClient({ decide: () => 'always' });
    const file = path.join(t.dir, 'approvals.json');
    try {
      const r = await t.js(`${APP} await app.getAXState({emit: false}); "a"`);
      assert.match(t.text(r), /=> a/);
      const notes = { app_id: 'org.example.Notes', executable: '/usr/bin/org.example.Notes' };
      const calc = { app_id: 'org.example.Calc', executable: '/usr/bin/org.example.Calc' };
      assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')), { always_allowed: [notes] });
      // A new server process with the same file does not ask again.
      const t2 = await startClient({ decide: () => 'decline', env: { CUA_APPROVALS_FILE: file } });
      try {
        const r2 = await t2.js(`${APP} await app.click(1); "b"`);
        assert.match(t2.text(r2), /=> b/);
        assert.equal(t2.elicitations.length, 0);
        // Regression: the file used to be re-read on every check, so anything
        // running as the user could add itself mid-session.
        writeFileSync(file, JSON.stringify({ always_allowed: [notes, calc] }));
        const r3 = await t2.js('try { await (await cua.getApp({windowId: 12}, {emit: false})).getAXState() } catch (e) { e.code }');
        assert.match(t2.text(r3), /=> approval_denied/);
        assert.equal(t2.elicitations.length, 1, 'a file change mid-session does not grant access');
      } finally {
        await t2.close();
      }
      // Entries bind the app ID to its executable.
      writeFileSync(file, JSON.stringify({ always_allowed: [{ ...calc, executable: '/usr/bin/impostor' }] }));
      const t4 = await startClient({ decide: () => 'decline', env: { CUA_APPROVALS_FILE: file } });
      try {
        const r4 = await t4.js('try { await (await cua.getApp({windowId: 12}, {emit: false})).getAXState() } catch (e) { e.code }');
        assert.match(t4.text(r4), /=> approval_denied/);
        assert.equal(t4.elicitations.length, 1);
      } finally {
        await t4.close();
      }
      writeFileSync(file, JSON.stringify({ always_allowed: [notes, calc] }));
      const t3 = await startClient({ decide: () => 'accept', env: { CUA_APPROVALS_FILE: file, CUA_DENY_APPS: 'org.example.Calc' } });
      try {
        const r3 = await t3.js('try { await (await cua.getApp({windowId: 12}, {emit: false})).getAXState() } catch (e) { e.code }');
        assert.match(t3.text(r3), /=> app_denied/);
        assert.equal(t3.events().filter((e) => e.method === 'approve_app').length, 0);
      } finally {
        await t3.close();
      }
    } finally {
      await t.close();
    }
  });
});

describe('approval identity', () => {
  test('turn and session approvals bind the app ID to its executable', async () => {
    const windows = [
      { window_id: 1, app_id: 'Same', title: 'a', executable: '/usr/bin/real' },
      { window_id: 2, app_id: 'Same', title: 'b', executable: '/usr/bin/real' },
      { window_id: 3, app_id: 'Same', title: 'c', executable: '/tmp/impostor' },
    ];
    const asked = [];
    const host = new HostApi({ engine: stubEngine(windows), elicit: async (d) => (asked.push(d.window_id), 'session') });
    for (const window_id of [1, 2, 3]) await host.call('get_ax_state', { window_id });
    assert.deepEqual(asked, [1, 3], 'same app ID with another executable is asked again');
  });

  test('an unreadable, malformed or old-format approvals file is ignored, never fatal or overwritten', async () => {
    const dir = mkdtempSync(path.join(tmpdir(), 'cua-approvals-'));
    try {
      for (const body of ['{not json', JSON.stringify({ always_allowed_app_ids: ['Same'] })]) {
        const file = path.join(dir, 'approvals.json');
        writeFileSync(file, body);
        const logs = [];
        const host = new HostApi({ engine: stubEngine([{ window_id: 1, app_id: 'Same', title: '', executable: '/usr/bin/real' }]), approvalsFile: file, log: (m) => logs.push(m), elicit: async () => 'always' });
        assert.match(logs.join('\n'), /ignoring approvals file/);
        assert.equal(host.store.available, false);
        await host.call('get_ax_state', { window_id: 1 }); // asked, approved for the session
        assert.equal(readFileSync(file, 'utf8'), body, 'file left unchanged');
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
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
      const r = await t.js(`try { ${APP} await app.click(1) } catch (e) { e.code }`);
      assert.match(t.text(r), /=> approval_unavailable/);
      assert.equal(t.performed().length, 0);
    } finally {
      await t.close();
    }
  });

  test('daemon deny list wins and choosing Deny is a denial', async () => {
    const t = await startClient({ env: { CUA_DENY_APPS: 'org.example.Calc' }, decide: () => 'deny-choice' });
    try {
      let r = await t.js('try { await (await cua.getApp({windowId: 12}, {emit: false})).getAXState() } catch (e) { e.code }');
      assert.match(t.text(r), /=> app_denied/);
      r = await t.js(`try { ${APP} await app.getAXState() } catch (e) { e.code }`);
      assert.match(t.text(r), /=> approval_denied/);
      assert.equal(t.events().filter((e) => e.method === 'approve_app').length, 0);
    } finally {
      await t.close();
    }
  });
});

// Real-engine regression tests against real GTK apps (xed, gnome-calculator)
// on a private display. Opt-in: run with the private display's environment
// (DISPLAY, XAUTHORITY, DBUS_SESSION_BUS_ADDRESS, GTK_MODULES=atk-bridge) and
//   CUA_REAL_ENGINE=/path/to/linux-cua node --test test/real-apps.test.js
// The apps are started and closed by the test.
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { startClient, sleep } from './helpers.js';

const ENGINE = process.env.CUA_REAL_ENGINE;
const skip = !ENGINE ? 'set CUA_REAL_ENGINE and a private display environment' : /^:0(\.|$)/.test(process.env.DISPLAY || ':0') ? 'never on display :0' : false;

describe('real apps', { skip }, () => {
  let t, dir;
  const procs = [];
  const launchedPids = [];
  const launch = (cmd, args, extraEnv = {}) => {
    const p = spawn(cmd, args, { cwd: dir, env: { ...process.env, ...extraEnv }, stdio: 'ignore' });
    procs.push(p);
    return p;
  };
  const run = async (code) => {
    const r = await t.js(code, { timeout_ms: 30000 });
    return { r, text: t.text(r) };
  };
  // Id of the first window matching `pred` (source of a JS predicate).
  const waitWindow = async (pred) => {
    for (let i = 0; i < 100; i++) {
      const { text } = await run(`(await cua.listWindows({emit: false})).find(${pred})?.id ?? 0`);
      const id = Number(text.match(/=> (\d+)/)?.[1]);
      if (id) return id;
      await sleep(100);
    }
    throw new Error(`no window matches ${pred}`);
  };

  before(async () => {
    dir = mkdtempSync(path.join(tmpdir(), 'cua-real-'));
    writeFileSync(path.join(dir, 'note.txt'), 'hello\n');
    // Two entries, so the chooser does not inline-complete the folder path
    // to its only file.
    writeFileSync(path.join(dir, 'other.txt'), 'other\n');
    launch('xed', ['--standalone', path.join(dir, 'note.txt')]);
    launch('gnome-calculator', [], { LC_NUMERIC: 'de_DE.UTF-8', LC_ALL: '' });
    t = await startClient({ env: { ...process.env, CUA_ENGINE: ENGINE, CUA_DISPLAY: process.env.DISPLAY, CUA_ALLOW_APPS: 'xed,gnome-calculator' } });
    // Bound by window: other xed or calculator processes may be running.
    const xid = await waitWindow(`(w) => w.app === "Xed" && w.title.includes(${JSON.stringify(path.basename(dir))})`);
    const cid = await waitWindow(`(w) => w.app === "Gnome-calculator"`);
    await run(`globalThis.x = await cua.getApp({windowId: ${xid}}, {emit: false}); globalThis.c = await cua.getApp({windowId: ${cid}}, {emit: false}); 1`);
  });
  after(async () => {
    await t?.close();
    for (const p of procs) p.kill();
    for (const pid of launchedPids) { try { process.kill(pid); } catch {} }
    rmSync(dir, { recursive: true, force: true });
  });

  test('a coordinate click after a shortcut opened an unseen dialog is refused', async () => {
    const { text } = await run(`await x.getScreenshot({emit: false}); await x.pressKey("ctrl+o");
      try { await x.click([300, 200]); "clicked" } catch (e) { e.code }`);
    assert.match(text, /=> input_refused/);
    // The cell's final state showed the dialog; after it closed, coordinates
    // work again once the model has looked at the window.
    const back = await run(`await x.pressKey("Escape"); await x.getScreenshot({emit: false}); await x.click([300, 200]); await x.scroll([300, 200], "down"); "clicked"`);
    assert.match(back.text, /=> clicked/, back.text);
  });

  test('confirm keys in a dialog that stays open do not wait for it to close; closes still land', async () => {
    let { text } = await run(`await x.pressKey("ctrl+o"); await x.pressKey("ctrl+l"); await x.typeText(${JSON.stringify(dir + '/')});
      const t0 = Date.now(); await x.pressKey("Return");
      [Date.now() - t0, (await cua.listWindows({emit: false})).filter((w) => w.title === "Open Files").length].join()`);
    // (one xed process: the dialog is this test's)
    const [ms, open] = text.match(/=> (\d+),(\d+)/).slice(1).map(Number);
    assert.equal(open, 1, 'the file chooser is still open');
    assert.ok(ms < 950, `stays-open Return took ${ms} ms`);
    ({ text } = await run(`await x.pressKey("ctrl+l"); await x.typeText(${JSON.stringify(path.join(dir, 'note.txt') + '\n')});
      (await cua.listWindows({emit: false})).filter((w) => w.title === "Open Files").length`));
    assert.match(text, /=> 0/, 'the trailing newline confirmed and closed the dialog');
  });

  test('text with a line break inside is refused for a single-line field', async () => {
    const { text } = await run(`await x.pressKey("ctrl+o"); await x.pressKey("ctrl+l");
      let code; try { await x.typeText("a\\nb") } catch (e) { code = e.code + ": " + e.message } await x.pressKey("Escape"); await x.pressKey("Escape"); code`);
    assert.match(text, /=> invalid_params: [^\n]*line break inside/);
  });

  test('text is never key-typed into an alert', async () => {
    const { text } = await run(`await x.typeText("more"); await x.pressKey("ctrl+w");
      let code; try { await x.typeText("/tmp/should-not-be-typed\\n") } catch (e) { code = e.code }
      code`);
    assert.match(text, /=> input_refused/);
    assert.ok(!existsSync('/tmp/should-not-be-typed'));
    await run(`const s = await x.getAXState({emit: false, disableDiffing: true});
      await x.click(Number(s.match(/\\[(\\d+)\\] button "Cancel"/)[1])); await x.pressKey("ctrl+z")`);
  });

  test('a decimal-comma locale is announced up front, and a reformatted setValue is a warning', async () => {
    // An observation the model does not receive does not use up the hint.
    await run('await c.getAXState({emit: false}); 1');
    const { text } = await run(`const c2 = await cua.getApp({windowId: c.windowId}); 1`);
    assert.match(text, /hint: number input: [^\n]*125,50/);
    const { r, text: t2 } = await run(`const s = await c.getAXState({emit: false, disableDiffing: true});
      await c.setValue(Number(s.match(/\\[(\\d+)\\] editbar/)[1]), "1.5"); "set"`);
    assert.equal(r.isError, false, t2);
    assert.match(t2, /=> set/);
    assert.match(t2, /decimal separator is a comma/);
  });

  test('a shift-click extends a file list selection to a range', async () => {
    const { r, text } = await run(`await x.pressKey("ctrl+o"); await x.pressKey("ctrl+l"); await x.typeText(${JSON.stringify(dir + '/')}); await x.pressKey("Return");
      const s = await x.getAXState({emit: false, disableDiffing: true});
      const cells = [...s.matchAll(/\\[(\\d+)\\] table cell "(?:note|other)\\.txt"/g)].map((m) => Number(m[1]));
      await x.click(cells[0]); await x.click(cells[1], {modifiers: "shift"});
      const after = await x.getAXState({emit: false, disableDiffing: true});
      const selected = [...after.matchAll(/\\[\\d+\\] table cell "(?:note|other)\\.txt" \\([^)]*selected/g)].length;
      await x.pressKey("Escape");
      [cells.length, selected].join()`);
    assert.equal(r.isError, false, text);
    assert.match(text, /=> 2,2/);
  });

  test('hover moves the virtual pointer; a relative move needs a position first', async () => {
    const { text } = await run(`let code; try { await c.moveMouseRelative(5, 5) } catch (e) { code = e.code }
      await x.getScreenshot({emit: false}); await x.moveMouse([300, 200]); await x.moveMouseRelative(10, -10); [code, "moved"].join()`);
    assert.match(text, /=> invalid_params,moved/);
  });

  test('getAXStateAndScreenshot is one engine request', async () => {
    const { text } = await run(`const {state, screenshot} = await x.getAXStateAndScreenshot({emit: false}); [typeof state, screenshot.length > 1000].join()`);
    assert.match(text, /=> string,true/);
    assert.match(text, /host calls: get_ax_state$/m);
  });

  test('the clipboard round-trips with the app in both directions', async () => {
    const { r, text } = await run(`await cua.clipboard.write("Grüße λ clip"); await x.pressKey("ctrl+v");
      const s = await x.getAXState({emit: false, disableDiffing: true});
      await x.pressKey("ctrl+a"); await x.pressKey("ctrl+c");
      const back = await cua.clipboard.read();
      await x.pressKey("ctrl+z");
      [s.includes("Grüße λ clip"), back.includes("Grüße λ clip"), back.includes("hello")].join()`);
    assert.equal(r.isError, false, text);
    assert.match(text, /=> true,true,true/);
  });

  test('launchApp starts an installed application and hands back its App', async () => {
    const { r, text } = await run(`const d = await cua.launchApp("drawing", {emit: false}); [d.id, d.windows.length > 0, d.pid].join()`);
    assert.equal(r.isError, false, text);
    const m = text.match(/=> Drawing,true,(\d+)/);
    assert.ok(m, text);
    // The test owns the process the engine started for it.
    launchedPids.push(Number(m[1]));
    const bad = await run(`try { await cua.launchApp("no-such-application") } catch (e) { e.code }`);
    assert.match(bad.text, /=> not_found/);
    const denied = await run(`try { await cua.launchApp("xterm") } catch (e) { e.code }`);
    assert.match(denied.text, /=> (app_denied|not_found)/);
  });
});

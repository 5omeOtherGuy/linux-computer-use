// Opt-in regression for continuous strokes against Drawing on a private display.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { startClient, sleep } from './helpers.js';
import { PNG } from 'pngjs';

const engine = process.env.CUA_REAL_ENGINE;
const skip = !engine || /^:0(\.|$)/.test(process.env.DISPLAY || ':0');
test('Drawing applies numeric tool size and complete strokes; invalid paths draw nothing', { skip }, async () => {
  const proc = spawn('drawing', [], { env: process.env, stdio: 'ignore' });
  let t;
  try {
    t = await startClient({ env: { ...process.env, CUA_ENGINE: engine, CUA_DISPLAY: process.env.DISPLAY, CUA_ALLOW_APPS: 'drawing' } });
    const run = async (code) => {
      const r = await t.js(code);
      assert.equal(r.isError, false, t.text(r));
      return r;
    };
    for (let i = 0; i < 100; i++) {
      const r = await run('(await cua.listApps({emit:false})).some(a => a.id.toLowerCase() === "drawing")');
      if (t.text(r).includes('=> true')) break;
      await sleep(100);
    }
    await run("const d = await cua.getApp('drawing')");
    let dismissed = false;
    for (let i = 0; i < 50; i++) {
      const r = await run(String.raw`const state = await d.getAXState({emit:false, disableDiffing:true});
        const no = state.match(/\[(\d+)\] button "No"/);
        if (no) { await d.click(Number(no[1])); true } else { false }`);
      if (t.text(r).includes('=> true')) { dismissed = true; break; }
      await sleep(100);
    }
    assert.ok(dismissed, 'dismissed the first-run help prompt');
    const sized = await run(String.raw`const controls = await d.getAXState({emit:false, disableDiffing:true});
      const size = controls.match(/\[(\d+)\] spin button[^\n]*desc="Tool size"/);
      if (!size) throw Error('no tool-size spin button');
      await d.setValue(Number(size[1]), '8'); await d.getAXState({disableDiffing:true});`);
    assert.match(t.text(sized), /spin button[^\n]*desc="Tool size" value=8 /, 'numeric value was committed, not only its editable text');
    const before = await run('await d.getScreenshot()');
    const image = (r) => PNG.sync.read(Buffer.from(r.content.find(c => c.type === 'image').data, 'base64'));
    const initial = image(before);
    // Window-local canvas coordinates from the initial capture. This test
    // compares changed pixels along each edge, not Drawing's image origin.
    await run('await d.drag({path:[[300,200],[480,200],[480,360],[300,360],[300,200]]})');
    const after = image(await run('await d.getScreenshot()'));
    if (process.env.CUA_TEST_SCREENSHOT) {
      writeFileSync(process.env.CUA_TEST_SCREENSHOT + '.before.png', PNG.sync.write(initial));
      writeFileSync(process.env.CUA_TEST_SCREENSHOT + '.after.png', PNG.sync.write(after));
    }
    const changed = (x,y) => {
      // Drawing smooths pencil paths into curves, which can bow away
      // from the requested straight edges. Look near each edge midpoint.
      for (let dy=-20;dy<=20;dy++) for (let dx=-20;dx<=20;dx++) {
        const k=((y+dy)*after.width+x+dx)*4;
        if (!after.data.subarray(k,k+3).equals(initial.data.subarray(k,k+3))) return true;
      }
      return false;
    };
    for (const [x,y] of [[390,200],[480,280],[390,360],[300,280]]) {
      assert.ok(changed(x,y), `missing stroke at ${x},${y}`);
    }
    // Two-point strokes also need their motion consumed before release.
    // Without pacing, Drawing can lose edges or connect separate drags.
    await run('await d.pressKey("alt+l"); await d.drag([550,200],[680,200]); await d.drag([680,200],[680,320]); await d.drag([680,320],[550,320]); await d.drag([550,320],[550,200])');
    const lines = image(await run('await d.getScreenshot()'));
    for (const [x,y] of [[615,200],[680,260],[615,320],[550,260]]) {
      const k=(y*lines.width+x)*4;
      assert.ok(!lines.data.subarray(k,k+3).equals(after.data.subarray(k,k+3)), `missing straight stroke at ${x},${y}`);
    }
    const bad = await t.js('await d.drag({path:[[740,200],[740,300],[950,300]]})');
    assert.equal(bad.isError, true, t.text(bad));
    const rejected = image(await run('await d.getScreenshot()'));
    // The invalid last point must be found before the first segment draws.
    for (let y=210;y<290;y++) for (let x=735;x<745;x++) {
      const k=(y*after.width+x)*4;
      assert.deepEqual(rejected.data.subarray(k,k+3), lines.data.subarray(k,k+3));
    }
  } finally {
    await t?.close();
    proc.kill();
  }
});

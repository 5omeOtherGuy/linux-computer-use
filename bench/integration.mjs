// Real MCP + QuickJS + Rust + GTK integration. Run with bench/fixture.py open
// on your owned private display; no fixture auto-approval beyond this test app.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { startClient } from '../surface/test/helpers.js';
import { HostApi } from '../surface/src/host.js';
import { OpenAIComputerTool } from '../surface/src/adapters/openai.js';
import { AnthropicComputerToolset } from '../surface/src/adapters/anthropic.js';

assert.ok(process.env.CUA_DISPLAY && process.env.CUA_ENGINE, 'CUA_DISPLAY and CUA_ENGINE are required');
assert.equal(process.env.DISPLAY, process.env.CUA_DISPLAY, 'Test input must target the same private display as the daemon');
assert.ok(!/:0(?:\.|$)/.test(process.env.DISPLAY), 'Never test on display 0');
const t = await startClient({ env: process.env, decide: (p) => p.message.includes('LinuxCuaFixture') ? 'accept' : 'decline' });
const run = async (code) => {
  const r = await t.js(code, { title: 'Integration', timeout_ms: 30000 });
  assert.equal(r.isError, false, t.text(r));
  return r;
};
let windowId;
try {
  let r = await run(`const w = (await cua.listWindows()).find(w => w.title === 'Linux CUA Benchmark');
    const app = await cua.getApp({windowId:w.id});
    const tree = await app.getAXState({disableDiffing:true});
    const field = Number(tree.match(/\\[(\\d+)\\] text "Message"/)[1]);
    const button = Number(tree.match(/\\[(\\d+)\\] button "Apply"/)[1]);
    await app.setValue(field, 'MCP Grüße λ');
    await app.click(button);
    w.id`);
  assert.match(t.text(r), /Applied: MCP Grüße λ/);
  windowId = Number(t.text(r).match(/=> (\d+)/)[1]);
  assert.equal(t.elicitations.length, 1);
  r = await run(`await app.performSecondaryAction(field, 'focus'); await app.setValue(field, 'Typed: '); await app.typeText('ä🙂'); await app.paste(' pasted'); await app.click(button);`);
  assert.match(t.text(r), /Applied:.*ä🙂.*pasted/);
  if (process.env.CUA_TEST_GUARD) {
    const guard = execFileSync('xdotool', ['search', '--name', '^CUA Background Guard$']).toString().trim();
    execFileSync('xdotool', ['windowactivate', '--sync', guard]);
  }
  const pointerBefore = execFileSync('xdotool', ['getmouselocation', '--shell']).toString();
  const focusBefore = execFileSync('xdotool', ['getwindowfocus']).toString();
  r = await run(`await app.setValue(field, 'Coordinate click');
    const fresh = await app.getAXState({disableDiffing:true});
    const b = fresh.match(/button "Apply"[^\\n]*@(\\d+),(\\d+) (\\d+)x(\\d+)/);
    await app.click([Number(b[1])+Math.floor(Number(b[3])/2),Number(b[2])+Math.floor(Number(b[4])/2)]);`);
  assert.match(t.text(r), /Applied: Coordinate click/);
  assert.equal(execFileSync('xdotool', ['getmouselocation', '--shell']).toString(), pointerBefore);
  assert.equal(execFileSync('xdotool', ['getwindowfocus']).toString(), focusBefore);
  // Consecutive captures exercise reuse of the shared-memory mapping.
  for (let i=0; i<3; i++) {
    r = await run('await app.getScreenshot()');
    assert.ok(r.content.some(c => c.type === 'image' && c.data.length > 100));
    if (process.env.CUA_TEST_SCREENSHOT) writeFileSync(process.env.CUA_TEST_SCREENSHOT, Buffer.from(r.content.find(c => c.type === 'image').data, 'base64'));
  }
  await t.client.callTool({name:'turn_ended', arguments:{}});
  await run('await app.getAXState({disableDiffing:true})');
  assert.equal(t.elicitations.length, 2);
  // Only the owned private display is used. XTEST input serves as a user-input
  // surrogate: the daemon must latch it and refuse the next action.
  execFileSync('xdotool', ['key', 'Shift_L']);
  await new Promise(resolve => setTimeout(resolve, 30));
  r = await t.js('await app.click(button)', {title:'Abort check'});
  assert.equal(r.isError, true);
  assert.match(t.text(r), /user_input_abort/);
  console.log('PASS: MCP elicitation, persistent getApp, Unicode edit/type/paste, action observations, screenshot reuse, turn reset');
} finally { await t.close(); }

const host = new HostApi({engineOptions:{command:process.env.CUA_ENGINE,display:process.env.CUA_DISPLAY},
  elicit: async ({app_id}) => app_id === 'LinuxCuaFixture' ? 'accept' : 'decline'});
try {
  const openai = new OpenAIComputerTool({host,windowId});
  const o = await openai.handleCall({type:'computer_call',call_id:'integration',actions:[{type:'screenshot'}]});
  assert.equal(o.error, undefined, o.error?.message);
  assert.equal(o.output.type, 'computer_call_output');
  const anthropic = new AnthropicComputerToolset({host,windowId});
  const a = await anthropic.runBatch([{type:'tool_use',toolset_name:'computer',name:'screenshot',id:'integration',input:{}}]);
  assert.equal(a[0].is_error ?? false, false);
  assert.ok(a[0].content.some(c => c.type === 'image'));
  console.log('PASS: both provider adapters return real window captures (no paid model inference)');
} finally { await host.close(); }

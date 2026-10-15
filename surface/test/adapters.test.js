// Provider adapter mappings against a recording host (no engine).
import { test, describe } from 'node:test';
import assert from 'node:assert/strict';
import { PNG } from 'pngjs';
import { AnthropicComputerToolset, HALT_TEXT, MEMBERS, TOOLSET_TYPE } from '../src/adapters/anthropic.js';
import { OpenAIComputerTool, normalizeKey } from '../src/adapters/openai.js';
import { ANTHROPIC_LEGACY_LIMITS, ANTHROPIC_TOOLSET_LIMITS, ScreenGeometry, fitSize } from '../src/adapters/geometry.js';
import { validateRequest } from '../src/protocol.js';

function png(w, h) {
  const p = new PNG({ width: w, height: h });
  p.data.fill(200);
  return PNG.sync.write(p).toString('base64');
}

function pngSize(b64) {
  const p = PNG.sync.read(Buffer.from(b64, 'base64'));
  return [p.width, p.height];
}

// Records calls and validates them exactly like HostApi does.
function recordingHost({ width = 400, height = 300, extensions = new Set(), fail = {} } = {}) {
  const shot = { data: png(width, height), mimeType: 'image/png', width, height };
  const host = {
    calls: [],
    async call(method, params) {
      const p = validateRequest(method, params, extensions);
      host.calls.push({ method, params: p });
      if (fail[method]) throw Object.assign(new Error(fail[method]), { code: 'engine_error' });
      if (method === 'list_windows') return [{ window_id: 7, title: 't', app_id: 'a', pid: 1, width, height }];
      if (method === 'get_screenshot') return shot;
      return { window: { window_id: 7 }, accessibility: '', ax_diff: '', timing: {}, warnings: [] };
    },
    actions: () => host.calls.filter((c) => !['list_windows', 'get_screenshot'].includes(c.method)),
  };
  return host;
}

const use = (id, name, input = {}) => ({ type: 'tool_use', id, name, toolset_name: 'computer', input });

describe('geometry', () => {
  test('fits the toolset visual-token and long-edge limits', () => {
    const { width, height } = fitSize(3000, 2000, ANTHROPIC_TOOLSET_LIMITS);
    assert.ok(Math.ceil(width / 28) * Math.ceil(height / 28) <= 4784);
    assert.ok(Math.max(width, height) <= 2576);
    assert.ok(Math.abs(width / height - 1.5) < 0.01, 'aspect ratio kept');
    assert.deepEqual(fitSize(1280, 800, ANTHROPIC_TOOLSET_LIMITS), { width: 1280, height: 800 });
    const legacy = fitSize(1512, 982, ANTHROPIC_LEGACY_LIMITS);
    assert.ok(legacy.width * legacy.height <= 1_150_000 && Math.max(legacy.width, legacy.height) <= 1568);
  });

  test('maps screenshot pixels to window pixels and back', () => {
    const g = new ScreenGeometry(2000, 1000, { maxLongEdge: 1000 });
    assert.equal(g.width, 1000);
    assert.deepEqual(g.toWindow([0, 0]), [1, 1]);
    assert.deepEqual(g.toWindow([500, 250]), [1001, 501]);
    assert.deepEqual(g.toWindow([999, 499]), [1999, 999]);
    assert.deepEqual(g.toScreenshot([1001, 501]), [500, 250]);
    assert.throws(() => g.toWindow([1000, 0]), /outside/);
    assert.throws(() => g.toWindow([-1, 0]), /outside/);
    assert.throws(() => g.toWindow(['1', 0]), /numbers/);
  });
});

describe('Anthropic computer_toolset_20260801', () => {
  test('tool definition disables members the engine cannot perform', () => {
    const t = new AnthropicComputerToolset({ host: recordingHost(), windowId: 7 });
    const def = t.toolDefinition();
    assert.equal(def.type, TOOLSET_TYPE);
    assert.equal(def.name, undefined, 'the toolset rejects name');
    assert.equal(def.display_width_px, undefined);
    const disabled = Object.keys(def.configs).sort();
    assert.deepEqual(disabled, ['double_click', 'hold_key', 'left_mouse_down', 'left_mouse_up', 'middle_click', 'mouse_move', 'right_click', 'triple_click']);
    assert.equal(MEMBERS.length, 17);
    const all = new AnthropicComputerToolset({ host: recordingHost(), windowId: 7, extensions: new Set(['click_button', 'click_count', 'mouse_move']), zoom: false });
    assert.deepEqual(Object.keys(all.toolDefinition().configs).sort(), ['hold_key', 'left_mouse_down', 'left_mouse_up', 'zoom']);
  });

  test('batch runs in order with toolset_name on every result and maps coordinates', async () => {
    const host = recordingHost({ width: 3000, height: 2000 });
    const t = new AnthropicComputerToolset({ host, windowId: 7 });
    const res = await t.runBatch([
      { type: 'text', text: 'plan' },
      use('a', 'screenshot'),
      use('b', 'left_click', { coordinate: [100, 50] }),
      use('c', 'type', { text: 'hello' }),
      use('d', 'key', { text: 'ctrl+s', repeat: 3 }),
      use('e', 'scroll', { scroll_direction: 'down', scroll_amount: 2 }),
      use('f', 'left_click_drag', { start_coordinate: [0, 0], coordinate: [10, 10] }),
      use('g', 'cursor_position'),
      use('h', 'wait', { duration: 0 }),
      { type: 'tool_use', id: 'other', name: 'bash', input: {} },
    ]);
    assert.equal(res.length, 8);
    for (const r of res) {
      assert.equal(r.toolset_name, 'computer');
      assert.equal(r.is_error, undefined, JSON.stringify(r.content));
    }
    assert.deepEqual(res.map((r) => r.tool_use_id), ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']);
    const img = res[0].content[0];
    assert.equal(img.type, 'image');
    assert.equal(img.source.media_type, 'image/png');
    const [w, h] = pngSize(img.source.data);
    assert.ok(Math.ceil(w / 28) * Math.ceil(h / 28) <= 4784, 'screenshot resized to limits');
    const g = t.geometry;
    const [cx, cy] = g.toWindow([100, 50]);
    assert.ok(cx > 100 && cy > 50, 'scaled up to window space');
    assert.deepEqual(host.actions().map((c) => [c.method, c.params]), [
      ['click', { window_id: 7, x: cx, y: cy }],
      ['type_text', { window_id: 7, text: 'hello' }],
      ['press_key', { window_id: 7, key: 'ctrl+s' }],
      ['press_key', { window_id: 7, key: 'ctrl+s' }],
      ['press_key', { window_id: 7, key: 'ctrl+s' }],
      ['scroll', { window_id: 7, x: cx, y: cy, direction: 'down', pixels: 200 }],
      ['drag', { window_id: 7, from: g.toWindow([0, 0]), to: g.toWindow([10, 10]) }],
    ]);
    assert.deepEqual(res[6].content, [{ type: 'text', text: `[${g.toScreenshot(g.toWindow([10, 10])).join(', ')}]` }]);
    assert.equal(res[1].content[0].text, 'OK');
    // batch did not end with a screenshot: one is attached to the last result
    assert.equal(res.at(-1).content.at(-1).type, 'image');
  });

  test('first failure halts the rest with the documented text', async () => {
    const host = recordingHost();
    const t = new AnthropicComputerToolset({ host, windowId: 7 });
    const res = await t.runBatch([
      use('1', 'left_click', { coordinate: [10, 10] }),
      use('2', 'right_click', { coordinate: [10, 10] }),
      use('3', 'type', { text: 'never' }),
      use('4', 'screenshot'),
    ]);
    assert.equal(res[0].is_error, undefined);
    assert.equal(res[1].is_error, true);
    assert.match(res[1].content, /unsupported/);
    for (const r of res.slice(2)) {
      assert.equal(r.is_error, true);
      assert.equal(r.content, HALT_TEXT);
      assert.equal(r.toolset_name, 'computer');
    }
    assert.equal(HALT_TEXT, 'Not executed: an earlier computer action in this turn failed.');
    assert.deepEqual(host.actions().map((c) => c.method), ['click']);
    assert.equal(res.at(-1).content, HALT_TEXT, 'no screenshot attached after a failure');
  });

  test('batch validation rejects malformed input before acting', async () => {
    const cases = [
      [use('x', 'left_click', { action: 'left_click', coordinate: [1, 1] }), /unexpected input field "action"/],
      [use('x', 'left_click', { coordinate: [400, 1] }), /outside/],
      [use('x', 'left_click', { coordinate: [1, 1], text: 'shift' }), /modifier/],
      [use('x', 'key', { text: 'Tab', repeat: 101 }), /repeat/],
      [use('x', 'scroll', { scroll_direction: 'diagonal', scroll_amount: 1, coordinate: [1, 1] }), /scroll_direction/],
      [use('x', 'wait', { duration: 301 }), /duration/],
      [use('x', 'zoom', { region: [10, 10, 5, 20] }), /region/],
      [use('x', 'hold_key', { text: 'shift', duration: 1 }), /not supported/],
      [use('x', 'left_mouse_down'), /not supported/],
      [use('x', 'left_click'), /pointer position is unknown/],
      [{ ...use('x', 'teleport'), name: 'teleport' }, /unknown computer member/],
    ];
    for (const [block, re] of cases) {
      const host = recordingHost();
      const t = new AnthropicComputerToolset({ host, windowId: 7 });
      const [r] = await t.runBatch([block]);
      assert.equal(r.is_error, true, JSON.stringify(block));
      assert.match(r.content, re);
      assert.equal(host.actions().length, 0, `no action for ${JSON.stringify(block.input)}`);
    }
  });

  test('zoom crops at full resolution and fits the screenshot box', async () => {
    const host = recordingHost({ width: 3000, height: 2000 });
    const t = new AnthropicComputerToolset({ host, windowId: 7, attachFinalScreenshot: false });
    await t.runBatch([use('s', 'screenshot')]);
    const box = [t.geometry.width, t.geometry.height];
    const [r] = await t.runBatch([use('z', 'zoom', { region: [0, 0, 200, 100] })]);
    const [w, h] = pngSize(r.content[0].source.data);
    assert.ok(w <= box[0] && h <= box[1]);
    assert.ok(Math.abs(w / h - 2) < 0.02, 'aspect ratio preserved');
  });

  test('modifiers and click variants map to engine extensions when enabled', async () => {
    const extensions = new Set(['click_button', 'click_count', 'modifiers']);
    const host = recordingHost({ extensions });
    const t = new AnthropicComputerToolset({ host, windowId: 7, extensions, attachFinalScreenshot: false });
    const res = await t.runBatch([
      use('1', 'right_click', { coordinate: [5, 5] }),
      use('2', 'triple_click', { coordinate: [5, 5], text: 'ctrl+shift' }),
      use('3', 'double_click'),
    ]);
    assert.ok(res.every((r) => !r.is_error), JSON.stringify(res));
    assert.deepEqual(host.actions().map((c) => c.params), [
      { window_id: 7, x: 5, y: 5, button: 'right' },
      { window_id: 7, x: 5, y: 5, click_count: 3, modifiers: ['ctrl', 'shift'] },
      { window_id: 7, x: 5, y: 5, click_count: 2 },
    ]);
  });

  test('confirm hook refuses before the block runs', async () => {
    const host = recordingHost();
    const t = new AnthropicComputerToolset({ host, windowId: 7, confirm: async (b) => b.name !== 'type' });
    const res = await t.runBatch([use('1', 'type', { text: 'buy now' }), use('2', 'screenshot')]);
    assert.match(res[0].content, /refused/);
    assert.equal(res[1].content, HALT_TEXT);
    assert.equal(host.actions().length, 0);
  });
});

describe('OpenAI computer tool', () => {
  const call = (actions, extra = {}) => ({ type: 'computer_call', id: 'cu_1', call_id: 'call_1', actions, pending_safety_checks: [], status: 'completed', ...extra });

  test('tool definition and key normalization', () => {
    const t = new OpenAIComputerTool({ host: recordingHost(), windowId: 7 });
    assert.deepEqual(t.toolDefinition(), { type: 'computer' });
    assert.equal(normalizeKey('ENTER'), 'Return');
    assert.equal(normalizeKey('ArrowLeft'), 'Left');
    assert.equal(normalizeKey('CTRL'), 'ctrl');
    assert.equal(normalizeKey('A'), 'a');
    assert.equal(normalizeKey('f5'), 'F5');
    assert.throws(() => normalizeKey('Hyper'), /unsupported key/);
  });

  test('maps a batched call in order and returns computer_call_output', async () => {
    const host = recordingHost({ width: 3200, height: 1800 });
    const slept = [];
    const t = new OpenAIComputerTool({ host, windowId: 7, limits: { maxWidth: 1600, maxHeight: 900 }, sleep: async (ms) => slept.push(ms) });
    const r = await t.handleCall(call([
      { type: 'screenshot' },
      { type: 'click', button: 'left', x: 405, y: 157 },
      { type: 'type', text: 'penguin' },
      { type: 'keypress', keys: ['CTRL', 'A'] },
      { type: 'keypress', keys: ['ENTER'] },
      { type: 'scroll', x: 10, y: 20, scroll_x: -30, scroll_y: 120 },
      { type: 'drag', path: [{ x: 1, y: 2 }, { x: 30, y: 40 }] },
      { type: 'wait' },
    ]));
    assert.equal(r.error, undefined, r.error?.message);
    assert.equal(r.executed, 8);
    assert.deepEqual(slept, [1000]);
    assert.deepEqual(host.actions().map((c) => [c.method, c.params]), [
      ['click', { window_id: 7, x: 811, y: 315 }],
      ['type_text', { window_id: 7, text: 'penguin' }],
      ['press_key', { window_id: 7, key: 'ctrl+a' }],
      ['press_key', { window_id: 7, key: 'Return' }],
      ['scroll', { window_id: 7, x: 21, y: 41, direction: 'down', pixels: 240 }],
      ['scroll', { window_id: 7, x: 21, y: 41, direction: 'left', pixels: 60 }],
      ['drag', { window_id: 7, from: [3, 5], to: [61, 81] }],
    ]);
    assert.equal(r.output.type, 'computer_call_output');
    assert.equal(r.output.call_id, 'call_1');
    assert.deepEqual(r.output.acknowledged_safety_checks, []);
    assert.equal(r.output.output.type, 'computer_screenshot');
    assert.equal(r.output.output.detail, 'original');
    assert.match(r.output.output.image_url, /^data:image\/png;base64,/);
    assert.deepEqual(pngSize(r.output.output.image_url.split(',')[1]), [1600, 900]);
  });

  test('invalid action anywhere in the batch: nothing runs', async () => {
    const bad = [
      [{ type: 'click', button: 'left', x: 1, y: 1 }, { type: 'click', button: 'back', x: 1, y: 1 }],
      [{ type: 'click', button: 'left', x: 1, y: 1 }, { type: 'double_click', x: 1, y: 1 }],
      [{ type: 'move', x: 1, y: 1 }],
      [{ type: 'click', button: 'left', x: 1.5, y: 1 }],
      [{ type: 'click', button: 'left', x: 1, y: 1, keys: ['SHIFT'] }],
      [{ type: 'type', text: 'a', extra: 1 }],
      [{ type: 'drag', path: [{ x: 1, y: 1 }] }],
      [{ type: 'drag', path: [{ x: 1, y: 1 }, { x: 2, y: 2 }, { x: 3, y: 3 }] }],
      [{ type: 'keypress', keys: [] }],
      [{ type: 'click', button: 'left', x: 1, y: 9999 }],
      [{ type: 'launch_missiles' }],
    ];
    for (const actions of bad) {
      const host = recordingHost();
      const t = new OpenAIComputerTool({ host, windowId: 7 });
      const r = await t.handleCall(call(actions));
      assert.ok(r.error, JSON.stringify(actions));
      assert.equal(r.executed, 0);
      assert.equal(host.actions().length, 0, JSON.stringify(actions));
      assert.equal(r.output.type, 'computer_call_output', 'state screenshot still returned');
    }
  });

  test('pending safety checks require confirmation and are acknowledged', async () => {
    const checks = [{ id: 'cu_sc_1', code: 'malicious_instructions', message: 'Detected instructions' }];
    let host = recordingHost();
    let t = new OpenAIComputerTool({ host, windowId: 7 });
    let r = await t.handleCall(call([{ type: 'click', button: 'left', x: 1, y: 1 }], { pending_safety_checks: checks }));
    assert.equal(r.refused, true);
    assert.equal(r.output, null);
    assert.equal(host.calls.length, 0, 'nothing runs without confirmation');

    host = recordingHost();
    const seen = [];
    t = new OpenAIComputerTool({ host, windowId: 7, confirmSafetyChecks: async (c) => (seen.push(c), true) });
    r = await t.handleCall(call([{ type: 'click', button: 'left', x: 1, y: 1 }], { pending_safety_checks: checks }));
    assert.deepEqual(seen, [checks]);
    assert.deepEqual(r.output.acknowledged_safety_checks, checks);
    assert.equal(host.actions().length, 1);
  });

  test('engine failure mid-batch stops and reports, with current screenshot', async () => {
    const host = recordingHost({ fail: { type_text: 'boom' } });
    const t = new OpenAIComputerTool({ host, windowId: 7 });
    const r = await t.handleCall(call([{ type: 'click', button: 'left', x: 1, y: 1 }, { type: 'type', text: 'x' }, { type: 'keypress', keys: ['ENTER'] }]));
    assert.equal(r.executed, 1);
    assert.match(r.error.message, /boom/);
    assert.deepEqual(host.actions().map((c) => c.method), ['click', 'type_text']);
    assert.ok(r.output.output.image_url);
  });

  test('legacy single action field is accepted', async () => {
    const host = recordingHost();
    const t = new OpenAIComputerTool({ host, windowId: 7 });
    const r = await t.handleCall({ type: 'computer_call', call_id: 'c', action: { type: 'type', text: 'hi' }, pending_safety_checks: [] });
    assert.equal(r.executed, 1);
    assert.deepEqual(host.actions()[0].params, { window_id: 7, text: 'hi' });
  });
});

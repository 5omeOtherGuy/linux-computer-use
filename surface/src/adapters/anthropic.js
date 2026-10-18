// Adapter for Anthropic's computer use client toolset `computer_toolset_20260801`
// (public docs: platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool).
//
// Claude calls members (`screenshot`, `left_click`, ...) as separate tool_use
// blocks carrying `toolset_name: "computer"`, often several per turn (a batch).
// This adapter targets one window of the engine: the "screen" is that window,
// screenshots are resized to the model's image limits and coordinates are
// mapped back to window-local pixels. Members the engine contract cannot
// perform are declared `enabled: false` in the toolset definition and are
// refused with is_error if called anyway; nothing is emulated.
import { CuaError, toCuaError } from '../errors.js';
import { ANTHROPIC_TOOLSET_LIMITS, ScreenGeometry, scaleShot, zoomShot } from './geometry.js';

export const TOOLSET_TYPE = 'computer_toolset_20260801';
export const TOOLSET_NAME = 'computer';
export const HALT_TEXT = 'Not executed: an earlier computer action in this turn failed.';
export const MEMBERS = Object.freeze([
  'screenshot', 'zoom', 'left_click', 'right_click', 'middle_click', 'double_click', 'triple_click',
  'left_click_drag', 'mouse_move', 'left_mouse_down', 'left_mouse_up', 'cursor_position',
  'scroll', 'type', 'key', 'hold_key', 'wait',
]);

const CLICKS = { left_click: { button: 'left', count: 1 }, right_click: { button: 'right', count: 1 }, middle_click: { button: 'middle', count: 1 }, double_click: { button: 'left', count: 2 }, triple_click: { button: 'left', count: 3 } };
const MODIFIER_ALIASES = { shift: 'shift', ctrl: 'ctrl', control: 'ctrl', alt: 'alt', super: 'super', cmd: 'super', command: 'super', meta: 'super', win: 'super' };
const DIRECTIONS = ['up', 'down', 'left', 'right'];

// Allowed input fields per member (strict: unknown fields are rejected).
const FIELDS = {
  screenshot: [], zoom: ['region'], left_click_drag: ['start_coordinate', 'coordinate', 'text'], mouse_move: ['coordinate'],
  left_mouse_down: [], left_mouse_up: [], cursor_position: [], scroll: ['scroll_direction', 'scroll_amount', 'coordinate', 'text'],
  type: ['text'], key: ['text', 'repeat'], hold_key: ['text', 'duration'], wait: ['duration'],
};
for (const m of Object.keys(CLICKS)) FIELDS[m] = ['coordinate', 'text'];

function invalid(msg) {
  return new CuaError('invalid_input', msg);
}

export class AnthropicComputerToolset {
  /**
   * @param {object} o
   * @param {{call: Function}} o.host        HostApi (or compatible) that validates/authorizes engine calls
   * @param {number} o.windowId              window the toolset drives
   * @param {Set<string>} [o.extensions]     accepted for compatibility; buttons, multi-clicks, held keys and moves are always available
   * @param {boolean} [o.zoom=true]          offer the zoom member
   * @param {object} [o.limits]              image limits (default: toolset-model limits)
   * @param {number} [o.scrollPixelsPerClick=100]
   * @param {boolean} [o.attachFinalScreenshot=true] add a screenshot to the last result of a batch that does not end with one
   * @param {Function} [o.confirm]           async (block) => boolean, asked before each block runs
   * @param {Function} [o.sleep]             async (ms, signal) => void (tests)
   */
  constructor({ host, windowId, extensions = new Set(), zoom = true, limits = ANTHROPIC_TOOLSET_LIMITS, scrollPixelsPerClick = 100, attachFinalScreenshot = true, confirm, sleep } = {}) {
    if (!host || typeof host.call !== 'function') throw new TypeError('host with call() is required');
    if (!Number.isInteger(windowId)) throw new TypeError('windowId must be an integer');
    this.host = host;
    this.windowId = windowId;
    this.extensions = extensions;
    this.zoomEnabled = zoom;
    this.limits = limits;
    this.scrollPixelsPerClick = scrollPixelsPerClick;
    this.attachFinalScreenshot = attachFinalScreenshot;
    this.confirm = confirm;
    this.sleep = sleep || defaultSleep;
    this.geometry = null;
    this.cursor = null; // window-local position of the engine's virtual pointer, as last set by this adapter
  }

  enabledMembers() {
    const on = new Set(['screenshot', 'left_click', 'right_click', 'middle_click', 'double_click', 'triple_click', 'left_click_drag', 'mouse_move', 'cursor_position', 'scroll', 'type', 'key', 'hold_key', 'wait']);
    if (this.zoomEnabled) on.add('zoom');
    // left_mouse_down and left_mouse_up have no engine equivalent (the
    // engine presses and releases within one action).
    return on;
  }

  // The `tools` entry for the Messages API. Members this engine cannot run
  // are disabled so Claude does not plan with them.
  toolDefinition({ cacheControl } = {}) {
    const on = this.enabledMembers();
    const configs = {};
    for (const m of MEMBERS) if (!on.has(m)) configs[m] = { enabled: false };
    const def = { type: TOOLSET_TYPE };
    if (Object.keys(configs).length) def.configs = configs;
    if (cacheControl) def.cache_control = cacheControl;
    return def;
  }

  async #ensureGeometry(signal) {
    if (this.geometry) return this.geometry;
    const windows = await this.host.call('list_windows', {}, { signal });
    const win = Array.isArray(windows) ? windows.find((w) => w && w.window_id === this.windowId) : null;
    if (!win) throw new CuaError('window_not_found', `window ${this.windowId} not found`);
    this.geometry = new ScreenGeometry(win.width, win.height, this.limits);
    return this.geometry;
  }

  async #screenshot(signal) {
    const shot = await this.host.call('get_screenshot', { window_id: this.windowId }, { signal });
    if (!shot || typeof shot.data !== 'string') throw new CuaError('bad_image', 'engine returned no screenshot');
    // Coordinates always refer to the latest screenshot returned to Claude.
    this.geometry = new ScreenGeometry(shot.width, shot.height, this.limits);
    return shot;
  }

  #modifiers(text, member) {
    if (text === undefined) return undefined;
    if (typeof text !== 'string' || !text) throw invalid(`${member}.text must be a modifier string such as "shift" or "ctrl+shift"`);
    const mods = text.toLowerCase().split('+').map((m) => MODIFIER_ALIASES[m.trim()]);
    if (mods.some((m) => !m)) throw invalid(`${member}.text must contain only shift, ctrl, alt or super joined with "+"`);
    return [...new Set(mods)];
  }

  // Structural validation that does not need geometry (run for every block first).
  validateBlock(block) {
    if (!block || block.type !== 'tool_use') throw invalid('not a tool_use block');
    if (block.toolset_name !== TOOLSET_NAME) throw invalid(`toolset_name must be "${TOOLSET_NAME}"`);
    if (typeof block.id !== 'string' || !block.id) throw invalid('tool_use block has no id');
    const name = block.name;
    if (!MEMBERS.includes(name)) throw invalid(`unknown computer member: ${String(name).slice(0, 64)}`);
    const input = block.input ?? {};
    if (typeof input !== 'object' || Array.isArray(input)) throw invalid(`${name}: input must be an object`);
    for (const k of Object.keys(input)) {
      if (!FIELDS[name].includes(k)) throw invalid(`${name}: unexpected input field "${String(k).slice(0, 64)}"`);
    }
    if (!this.enabledMembers().has(name)) throw new CuaError('unsupported', `${name} is not supported by this engine (disabled in the toolset definition)`);
    return { name, input };
  }

  async #run(name, input, signal) {
    const w = this.windowId;
    switch (name) {
      case 'screenshot': {
        const shot = await this.#screenshot(signal);
        const scaled = scaleShot(shot, this.geometry);
        return [imageBlock(scaled)];
      }
      case 'zoom': {
        const r = input.region;
        if (!Array.isArray(r) || r.length !== 4 || !r.every((v) => typeof v === 'number' && Number.isFinite(v))) throw invalid('zoom.region must be [x0, y0, x1, y1]');
        const g = await this.#ensureGeometry(signal);
        const [x0, y0, x1, y1] = r;
        if (!(x0 >= 0 && y0 >= 0 && x1 > x0 && y1 > y0 && x1 <= g.width && y1 <= g.height)) {
          throw new CuaError('out_of_bounds', `zoom.region [${r.join(', ')}] must satisfy 0 <= x0 < x1 <= ${g.width} and 0 <= y0 < y1 <= ${g.height}`);
        }
        const shot = await this.host.call('get_screenshot', { window_id: w }, { signal });
        const sx = shot.width / g.width, sy = shot.height / g.height;
        const region = [Math.floor(x0 * sx), Math.floor(y0 * sy), Math.max(Math.floor(x0 * sx) + 1, Math.ceil(x1 * sx)), Math.max(Math.floor(y0 * sy) + 1, Math.ceil(y1 * sy))];
        return [imageBlock(zoomShot(shot, region, g))];
      }
      case 'left_click': case 'right_click': case 'middle_click': case 'double_click': case 'triple_click': {
        const { button, count } = CLICKS[name];
        const modifiers = this.#modifiers(input.text, name);
        const g = await this.#ensureGeometry(signal);
        let pt;
        if (input.coordinate !== undefined) pt = g.toWindow(input.coordinate);
        else if (this.cursor) pt = this.cursor;
        else throw new CuaError('no_cursor', `${name} without coordinate: the pointer position is unknown; pass coordinate`);
        const params = { window_id: w, x: pt[0], y: pt[1], observe: false };
        if (button !== 'left') params.button = button;
        if (count !== 1) params.click_count = count;
        if (modifiers) params.modifiers = modifiers;
        await this.host.call('click', params, { signal });
        this.cursor = pt;
        return okText();
      }
      case 'left_click_drag': {
        const modifiers = this.#modifiers(input.text, name);
        const g = await this.#ensureGeometry(signal);
        if (input.start_coordinate === undefined || input.coordinate === undefined) throw invalid('left_click_drag needs start_coordinate and coordinate');
        const from = g.toWindow(input.start_coordinate, 'start_coordinate');
        const to = g.toWindow(input.coordinate);
        const params = { window_id: w, from, to, observe: false };
        if (modifiers) params.modifiers = modifiers;
        await this.host.call('drag', params, { signal });
        this.cursor = to;
        return okText();
      }
      case 'mouse_move': {
        const g = await this.#ensureGeometry(signal);
        if (input.coordinate === undefined) throw invalid('mouse_move needs coordinate');
        const pt = g.toWindow(input.coordinate);
        await this.host.call('mouse_move', { window_id: w, x: pt[0], y: pt[1], observe: false }, { signal });
        this.cursor = pt;
        return okText();
      }
      case 'cursor_position': {
        const g = await this.#ensureGeometry(signal);
        if (!this.cursor) throw new CuaError('no_cursor', 'pointer position is unknown: no pointer action has been performed yet');
        const [x, y] = g.toScreenshot(this.cursor);
        return [{ type: 'text', text: `[${x}, ${y}]` }];
      }
      case 'scroll': {
        const dir = input.scroll_direction;
        if (!DIRECTIONS.includes(dir)) throw invalid('scroll.scroll_direction must be up, down, left or right');
        const amount = input.scroll_amount;
        if (!Number.isInteger(amount) || amount < 1 || amount > 1000) throw invalid('scroll.scroll_amount must be an integer in [1, 1000]');
        const modifiers = this.#modifiers(input.text, name);
        const g = await this.#ensureGeometry(signal);
        let pt;
        if (input.coordinate !== undefined) pt = g.toWindow(input.coordinate);
        else if (this.cursor) pt = this.cursor;
        else throw new CuaError('no_cursor', 'scroll without coordinate: the pointer position is unknown; pass coordinate');
        const params = { window_id: w, x: pt[0], y: pt[1], direction: dir, pixels: amount * this.scrollPixelsPerClick, observe: false };
        if (modifiers) params.modifiers = modifiers;
        await this.host.call('scroll', params, { signal });
        this.cursor = pt;
        return okText();
      }
      case 'type': {
        if (typeof input.text !== 'string' || !input.text) throw invalid('type.text must be a non-empty string');
        await this.host.call('type_text', { window_id: w, text: input.text, observe: false }, { signal });
        return okText();
      }
      case 'key': {
        if (typeof input.text !== 'string' || !input.text) throw invalid('key.text must be a key or chord such as "Return" or "ctrl+s"');
        const repeat = input.repeat ?? 1;
        if (!Number.isInteger(repeat) || repeat < 1 || repeat > 100) throw invalid('key.repeat must be an integer from 1 to 100');
        for (let i = 0; i < repeat; i++) await this.host.call('press_key', { window_id: w, key: input.text, observe: false }, { signal });
        return okText();
      }
      case 'hold_key': {
        if (typeof input.text !== 'string' || !input.text) throw invalid('hold_key.text must be a key or chord such as "shift" or "ctrl+s"');
        const d = input.duration;
        if (typeof d !== 'number' || !(d >= 0) || d > 10) throw invalid('hold_key.duration must be a number of seconds from 0 to 10');
        await this.host.call('press_key', { window_id: w, key: input.text, hold_ms: Math.round(d * 1000), observe: false }, { signal });
        return okText();
      }
      case 'wait': {
        const d = input.duration;
        if (typeof d !== 'number' || !(d >= 0) || d > 300) throw invalid('wait.duration must be a number of seconds from 0 to 300');
        await this.sleep(d * 1000, signal);
        return okText();
      }
      default:
        throw new CuaError('unsupported', `${name} is not supported by this engine`);
    }
  }

  /**
   * Run the computer members of one assistant turn, in order, stopping at the
   * first failure. Returns one tool_result per computer tool_use block (other
   * tools' blocks are ignored), each echoing toolset_name.
   */
  async runBatch(content, { signal } = {}) {
    const blocks = (Array.isArray(content) ? content : []).filter((b) => b && b.type === 'tool_use' && b.toolset_name === TOOLSET_NAME);
    const ids = new Set();
    const results = [];
    let failed = false;
    let lastName = null;
    for (const block of blocks) {
      const base = { type: 'tool_result', tool_use_id: block.id, toolset_name: TOOLSET_NAME };
      if (failed) {
        results.push({ ...base, is_error: true, content: HALT_TEXT });
        continue;
      }
      try {
        if (ids.has(block.id)) throw invalid(`duplicate tool_use id ${block.id}`);
        ids.add(block.id);
        if (signal?.aborted) throw new CuaError('cancelled', 'batch cancelled');
        const { name, input } = this.validateBlock(block);
        if (this.confirm && !(await this.confirm(block))) throw new CuaError('refused', 'the user refused this action');
        const out = await this.#run(name, input, signal);
        results.push({ ...base, content: out });
        lastName = name;
      } catch (err) {
        const e = toCuaError(err);
        failed = true;
        results.push({ ...base, is_error: true, content: `${e.code}: ${e.message}` });
      }
    }
    if (!failed && this.attachFinalScreenshot && results.length && lastName !== 'screenshot' && lastName !== 'zoom') {
      try {
        const shot = await this.#screenshot(signal);
        results[results.length - 1].content.push(imageBlock(scaleShot(shot, this.geometry)));
      } catch {
        // Optional extra; the batch itself succeeded.
      }
    }
    return results;
  }

  // Call at the end of a user turn (mirrors the engine's turn_ended): forget
  // pointer state; geometry is re-derived from the next screenshot.
  reset() {
    this.cursor = null;
    this.geometry = null;
  }
}

function okText() {
  return [{ type: 'text', text: 'OK' }];
}

function imageBlock(img) {
  return { type: 'image', source: { type: 'base64', media_type: img.mimeType, data: img.data } };
}

export function defaultSleep(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new CuaError('cancelled', 'wait cancelled'));
    const t = setTimeout(resolve, ms);
    signal?.addEventListener('abort', () => {
      clearTimeout(t);
      reject(new CuaError('cancelled', 'wait cancelled'));
    }, { once: true });
  });
}

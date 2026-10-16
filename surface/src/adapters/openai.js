// Adapter for OpenAI's Responses API computer tool (`tools: [{ type: "computer" }]`)
// (public docs: developers.openai.com/api/docs/guides/tools-computer-use and
// .../tools-computer-use-integration).
//
// A `computer_call` carries an ordered `actions` array (older items carry a
// single `action`). Actions: click, double_click, drag, move, scroll,
// keypress, type, wait, screenshot. The whole batch is validated before any
// action runs; then actions run in order and the updated screenshot is
// returned as a `computer_call_output`. `pending_safety_checks` must be
// confirmed by a human before anything runs and are then acknowledged.
import { CuaError, toCuaError } from '../errors.js';
import { ScreenGeometry, scaleShot } from './geometry.js';
import { defaultSleep } from './anthropic.js';

export const TOOL_DEFINITION = Object.freeze({ type: 'computer' });
const BUTTONS = { left: 'left', right: 'right', wheel: 'middle' };
const ACTION_FIELDS = {
  click: ['type', 'button', 'x', 'y', 'keys'],
  double_click: ['type', 'x', 'y', 'keys'],
  drag: ['type', 'path', 'keys'],
  move: ['type', 'x', 'y', 'keys'],
  scroll: ['type', 'x', 'y', 'scroll_x', 'scroll_y', 'keys'],
  keypress: ['type', 'keys'],
  type: ['type', 'text'],
  wait: ['type'],
  screenshot: ['type'],
};

// Model key names -> X keysym / engine chord tokens.
const KEY_MAP = {
  ENTER: 'Return', RETURN: 'Return', ESC: 'Escape', ESCAPE: 'Escape', TAB: 'Tab', SPACE: 'space',
  BACKSPACE: 'BackSpace', DELETE: 'Delete', DEL: 'Delete', INSERT: 'Insert', HOME: 'Home', END: 'End',
  PAGEUP: 'Page_Up', PAGEDOWN: 'Page_Down', PGUP: 'Page_Up', PGDN: 'Page_Down',
  UP: 'Up', DOWN: 'Down', LEFT: 'Left', RIGHT: 'Right',
  ARROWUP: 'Up', ARROWDOWN: 'Down', ARROWLEFT: 'Left', ARROWRIGHT: 'Right',
  CTRL: 'ctrl', CONTROL: 'ctrl', SHIFT: 'shift', ALT: 'alt', OPTION: 'alt',
  META: 'super', CMD: 'super', COMMAND: 'super', SUPER: 'super', WIN: 'super',
  CAPSLOCK: 'Caps_Lock', PRINTSCREEN: 'Print', MENU: 'Menu',
};
const MODIFIER_TOKENS = { ctrl: 'ctrl', shift: 'shift', alt: 'alt', super: 'super' };

export function normalizeKey(name) {
  if (typeof name !== 'string' || !name) throw new CuaError('invalid_input', 'key names must be non-empty strings');
  const upper = name.toUpperCase();
  if (KEY_MAP[upper]) return KEY_MAP[upper];
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(upper)) return upper;
  // Single characters in a chord are keys, not text: "A" in ["CTRL", "A"] means ctrl+a.
  if (name.length === 1 && /[\x21-\x7e]/.test(name)) return name.toLowerCase();
  throw new CuaError('invalid_input', `unsupported key name: ${name.slice(0, 32)}`);
}

function invalid(msg) {
  return new CuaError('invalid_input', msg);
}

function isInt(v) {
  return Number.isInteger(v);
}

export class OpenAIComputerTool {
  /**
   * @param {object} o
   * @param {{call: Function}} o.host       HostApi (or compatible)
   * @param {number} o.windowId             window the tool drives
   * @param {Set<string>} [o.extensions]    engine extensions (click_button, click_count, modifiers, mouse_move)
   * @param {object} [o.limits]             optional downscale limits, e.g. { maxWidth: 1600, maxHeight: 900 }
   * @param {number} [o.waitMs=1000]        duration of a `wait` action
   * @param {Function} [o.confirmSafetyChecks] async (checks, call) => boolean; required to proceed when checks are pending
   * @param {string} [o.detail='original']  image detail for computer_screenshot
   * @param {boolean} [o.allowDragPathSimplification=false] run multi-point drag paths as first->last
   */
  constructor({ host, windowId, extensions = new Set(), limits = {}, waitMs = 1000, confirmSafetyChecks, detail = 'original', allowDragPathSimplification = false, sleep } = {}) {
    if (!host || typeof host.call !== 'function') throw new TypeError('host with call() is required');
    if (!Number.isInteger(windowId)) throw new TypeError('windowId must be an integer');
    this.host = host;
    this.windowId = windowId;
    this.extensions = extensions;
    this.limits = limits;
    this.waitMs = waitMs;
    this.confirmSafetyChecks = confirmSafetyChecks;
    this.detail = detail;
    this.allowDragPathSimplification = allowDragPathSimplification;
    this.sleep = sleep || defaultSleep;
    this.geometry = null;
  }

  toolDefinition() {
    return { ...TOOL_DEFINITION };
  }

  async #ensureGeometry(signal) {
    if (this.geometry) return this.geometry;
    const windows = await this.host.call('list_windows', {}, { signal });
    const win = Array.isArray(windows) ? windows.find((w) => w && w.window_id === this.windowId) : null;
    if (!win) throw new CuaError('window_not_found', `window ${this.windowId} not found`);
    this.geometry = new ScreenGeometry(win.width, win.height, this.limits);
    return this.geometry;
  }

  #modifiers(action) {
    if (action.keys === undefined || action.keys === null) return undefined;
    if (!Array.isArray(action.keys)) throw invalid(`${action.type}.keys must be an array`);
    if (action.keys.length === 0) return undefined;
    const mods = action.keys.map((k) => MODIFIER_TOKENS[normalizeKey(k)]);
    if (mods.some((m) => !m)) throw invalid(`${action.type}.keys may only hold modifier keys (CTRL, SHIFT, ALT, META)`);
    if (!this.extensions.has('modifiers')) throw new CuaError('unsupported', `${action.type} with held keys is not supported by this engine`);
    return [...new Set(mods)];
  }

  #point(g, x, y, name) {
    if (!isInt(x) || !isInt(y)) throw invalid(`${name} needs integer x and y`);
    return g.toWindow([x, y], name);
  }

  // Validate every action and translate it to engine requests before running
  // anything. Returns a list of steps: { method, params } | { wait } | { noop }.
  #plan(actions, g) {
    if (!Array.isArray(actions) || actions.length === 0) throw invalid('computer_call has no actions');
    if (actions.length > 100) throw invalid('too many actions in one computer_call');
    const steps = [];
    const w = this.windowId;
    actions.forEach((a, i) => {
      const where = `actions[${i}]`;
      const first = steps.length;
      if (!a || typeof a !== 'object' || typeof a.type !== 'string' || !ACTION_FIELDS[a.type]) throw invalid(`${where}: unknown action type ${JSON.stringify(a?.type)}`);
      for (const k of Object.keys(a)) if (!ACTION_FIELDS[a.type].includes(k)) throw invalid(`${where}: unexpected field "${String(k).slice(0, 64)}" for ${a.type}`);
      switch (a.type) {
        case 'click': {
          const button = BUTTONS[a.button ?? 'left'];
          if (!button) throw new CuaError('unsupported', `${where}: mouse button "${a.button}" is not supported`);
          if (button !== 'left' && !this.extensions.has('click_button')) throw new CuaError('unsupported', `${where}: ${a.button} click is not supported by this engine`);
          const [x, y] = this.#point(g, a.x, a.y, where);
          const params = { window_id: w, x, y };
          if (button !== 'left') params.button = button;
          const mods = this.#modifiers(a);
          if (mods) params.modifiers = mods;
          steps.push({ method: 'click', params });
          break;
        }
        case 'double_click': {
          if (!this.extensions.has('click_count')) throw new CuaError('unsupported', `${where}: double_click is not supported by this engine`);
          const [x, y] = this.#point(g, a.x, a.y, where);
          const params = { window_id: w, x, y, click_count: 2 };
          const mods = this.#modifiers(a);
          if (mods) params.modifiers = mods;
          steps.push({ method: 'click', params });
          break;
        }
        case 'drag': {
          if (!Array.isArray(a.path) || a.path.length < 2) throw invalid(`${where}: drag needs a path of at least two points`);
          const pts = a.path.map((p, j) => {
            const [px, py] = Array.isArray(p) ? p : [p?.x, p?.y];
            return this.#point(g, px, py, `${where}.path[${j}]`);
          });
          if (pts.length > 2 && !this.allowDragPathSimplification) {
            throw new CuaError('unsupported', `${where}: drag paths with intermediate points are not supported by this engine (start -> end only)`);
          }
          const params = { window_id: w, from: pts[0], to: pts[pts.length - 1] };
          const mods = this.#modifiers(a);
          if (mods) params.modifiers = mods;
          steps.push({ method: 'drag', params });
          break;
        }
        case 'move': {
          if (!this.extensions.has('mouse_move')) throw new CuaError('unsupported', `${where}: pointer move/hover is not supported by this engine`);
          if (this.#modifiers(a)) throw new CuaError('unsupported', `${where}: move with held keys is not supported`);
          const [x, y] = this.#point(g, a.x, a.y, where);
          steps.push({ method: 'mouse_move', params: { window_id: w, x, y } });
          break;
        }
        case 'scroll': {
          const [x, y] = this.#point(g, a.x, a.y, where);
          if (!isInt(a.scroll_x) || !isInt(a.scroll_y)) throw invalid(`${where}: scroll_x and scroll_y must be integers`);
          const mods = this.#modifiers(a);
          const add = (direction, pixels) => {
            const params = { window_id: w, x, y, direction, pixels };
            if (mods) params.modifiers = mods;
            steps.push({ method: 'scroll', params });
          };
          if (a.scroll_y) add(a.scroll_y > 0 ? 'down' : 'up', g.lengthToWindow(a.scroll_y, 'y'));
          if (a.scroll_x) add(a.scroll_x > 0 ? 'right' : 'left', g.lengthToWindow(a.scroll_x, 'x'));
          if (!a.scroll_x && !a.scroll_y) steps.push({ noop: true });
          break;
        }
        case 'keypress': {
          if (!Array.isArray(a.keys) || a.keys.length === 0 || a.keys.length > 8) throw invalid(`${where}: keypress needs 1-8 keys`);
          steps.push({ method: 'press_key', params: { window_id: w, key: a.keys.map(normalizeKey).join('+') } });
          break;
        }
        case 'type':
          if (typeof a.text !== 'string') throw invalid(`${where}: type needs text`);
          if (a.text) steps.push({ method: 'type_text', params: { window_id: w, text: a.text } });
          else steps.push({ noop: true });
          break;
        case 'wait':
          steps.push({ wait: this.waitMs });
          break;
        case 'screenshot':
          steps.push({ noop: true }); // the output screenshot is captured after the batch
          break;
        default:
          throw invalid(`${where}: unsupported action`);
      }
      for (let k = first; k < steps.length; k++) steps[k].action = i;
    });
    return steps;
  }

  /**
   * Handle one `computer_call` item.
   * Returns { output, executed, error?, refused? }:
   *  - output: the `computer_call_output` input item to send back (null when
   *    the call was refused and nothing ran);
   *  - executed: number of actions that completed (a failing action is not counted);
   *  - error: CuaError if validation or an action failed (output then still
   *    carries the current screenshot so the model can see the state).
   */
  async handleCall(call, { signal } = {}) {
    if (!call || call.type !== 'computer_call' || typeof call.call_id !== 'string') throw invalid('not a computer_call item');
    const actions = Array.isArray(call.actions) ? call.actions : call.action ? [call.action] : [];
    const checks = Array.isArray(call.pending_safety_checks) ? call.pending_safety_checks : [];
    let acknowledged = [];
    if (checks.length) {
      const ok = this.confirmSafetyChecks ? await this.confirmSafetyChecks(checks, call) : false;
      if (!ok) {
        return { output: null, executed: 0, refused: true, error: new CuaError('safety_check_refused', `pending safety checks not acknowledged: ${checks.map((c) => c.code).join(', ')}`) };
      }
      acknowledged = checks.map((c) => ({ id: c.id, ...(c.code ? { code: c.code } : {}), ...(c.message ? { message: c.message } : {}) }));
    }
    let executed = 0;
    let error;
    try {
      const g = await this.#ensureGeometry(signal);
      const steps = this.#plan(actions, g);
      for (const [idx, step] of steps.entries()) {
        if (signal?.aborted) throw new CuaError('cancelled', 'computer_call cancelled');
        // The screenshot after the batch is the only state read: no tree per action.
        if (step.method) await this.host.call(step.method, { ...step.params, observe: false }, { signal });
        else if (step.wait) await this.sleep(step.wait, signal);
        if (steps[idx + 1]?.action !== step.action) executed = step.action + 1; // actions fully completed
      }
    } catch (err) {
      error = toCuaError(err);
      if (error.code === 'cancelled') return { output: null, executed, error };
    }
    let img;
    try {
      const shot = await this.host.call('get_screenshot', { window_id: this.windowId }, { signal });
      // Coordinates in the next call refer to this screenshot.
      this.geometry = new ScreenGeometry(shot.width, shot.height, this.limits);
      img = scaleShot(shot, this.geometry);
    } catch (err) {
      return { output: null, executed, error: error || toCuaError(err) };
    }
    const output = {
      type: 'computer_call_output',
      call_id: call.call_id,
      acknowledged_safety_checks: acknowledged,
      output: { type: 'computer_screenshot', image_url: `data:${img.mimeType};base64,${img.data}`, detail: this.detail },
    };
    return error ? { output, executed, error } : { output, executed };
  }

  // Convenience: handle every computer_call in a response's output array.
  async handleResponseOutput(outputItems, { signal } = {}) {
    const results = [];
    for (const item of Array.isArray(outputItems) ? outputItems : []) {
      if (item?.type !== 'computer_call') continue;
      const r = await this.handleCall(item, { signal });
      results.push(r);
      if (r.error || r.refused) break;
    }
    return results;
  }

  reset() {
    this.geometry = null;
  }
}

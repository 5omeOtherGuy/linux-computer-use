// Method table for the engine contract (CONTRACT.md) and strict parameter
// validation. The host validates every request from model code, discrete MCP
// tools and provider adapters against this table before anything reaches the
// daemon. Unknown methods, unknown parameters and out-of-range values are
// rejected; the daemon still performs its own authorization.
import { CuaError } from './errors.js';

// Methods only the trusted host may send. Never exposed to model code or tools.
export const HOST_ONLY_METHODS = Object.freeze(['approve_app', 'turn_ended']);

// Former optional engine extensions (mouse buttons, multi-clicks, held
// modifiers, pointer moves). The daemon implements them all now, so they are
// part of the contract; the names are still accepted in CUA_ENGINE_EXTENSIONS
// for compatibility and change nothing.
export const KNOWN_EXTENSIONS = Object.freeze(['click_button', 'click_count', 'modifiers', 'mouse_move']);

export const MAX_TEXT = 100_000;
const MAX_COORD = 65_535; // window-local pixels; WindowInfo width/height are u16
const MAX_ELEMENT_INDEX = 10_000_000;
const MAX_HOLD_MS = 10_000;
const MODIFIERS = ['shift', 'ctrl', 'alt', 'super'];

function fail(method, message) {
  throw new CuaError('invalid_params', `${method}: ${message}`);
}

function isPlainObject(v) {
  if (v === null || typeof v !== 'object' || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}

const checks = {
  windowId(method, v) {
    if (!Number.isInteger(v) || v < 0 || v > 0xffffffff) fail(method, 'window_id must be an unsigned 32-bit integer');
    return v;
  },
  coord(method, name, v) {
    if (!Number.isInteger(v) || v < 0 || v > MAX_COORD) fail(method, `${name} must be an integer in [0, ${MAX_COORD}] (window-local pixels)`);
    return v;
  },
  point(method, name, v) {
    if (!Array.isArray(v) || v.length !== 2) fail(method, `${name} must be [x, y]`);
    return [checks.coord(method, `${name}[0]`, v[0]), checks.coord(method, `${name}[1]`, v[1])];
  },
  elementIndex(method, v) {
    if (!Number.isInteger(v) || v < 0 || v > MAX_ELEMENT_INDEX) fail(method, 'element_index must be a non-negative integer');
    return v;
  },
  text(method, name, v, { allowEmpty = false, max = MAX_TEXT } = {}) {
    if (typeof v !== 'string') fail(method, `${name} must be a string`);
    if (!allowEmpty && v.length === 0) fail(method, `${name} must not be empty`);
    if (v.length > max) fail(method, `${name} exceeds ${max} characters`);
    return v;
  },
  key(method, v) {
    checks.text(method, 'key', v, { max: 128 });
    // printable ASCII only: keysym names, single characters and '+' chords
    if (!/^[\x21-\x7e]+$/.test(v)) fail(method, 'key must be printable ASCII without spaces, e.g. "Return" or "ctrl+s"');
    return v;
  },
  label(method, name, v) {
    checks.text(method, name, v, { max: 256 });
    if (/[\x00-\x1f\x7f]/.test(v)) fail(method, `${name} must not contain control characters`);
    return v;
  },
  modifiers(method, v) {
    if (!Array.isArray(v) || v.length > 4) fail(method, 'modifiers must be an array of at most 4 names');
    const out = [];
    for (const m of v) {
      if (!MODIFIERS.includes(m)) fail(method, `modifier must be one of ${MODIFIERS.join(', ')}`);
      if (!out.includes(m)) out.push(m);
    }
    return out;
  },
  holdMs(method, v) {
    if (!Number.isInteger(v) || v < 0 || v > MAX_HOLD_MS) fail(method, `hold_ms must be an integer in [0, ${MAX_HOLD_MS}]`);
    return v;
  },
  bool(method, name, v) {
    if (typeof v !== 'boolean') fail(method, `${name} must be a boolean`);
    return v;
  },
};

// Target (element_index or x,y) shared by click and scroll.
function target(method, p, out) {
  const hasIndex = p.element_index !== undefined;
  const hasXY = p.x !== undefined || p.y !== undefined;
  if (hasIndex === hasXY) fail(method, 'provide exactly one of element_index or x,y');
  if (hasIndex) out.element_index = checks.elementIndex(method, p.element_index);
  else {
    if (p.x === undefined || p.y === undefined) fail(method, 'x and y are both required');
    out.x = checks.coord(method, 'x', p.x);
    out.y = checks.coord(method, 'y', p.y);
  }
}

// spec: allowed keys, validator producing a normalized fresh object.
const METHODS = {
  list_apps: { kind: 'read', keys: [], build: () => ({}) },
  list_windows: { kind: 'read', keys: [], build: () => ({}) },
  get_app_state: { kind: 'read', keys: ['window_id'], build: (m, p) => ({ window_id: checks.windowId(m, p.window_id) }) },
  get_ax_state: { kind: 'read', keys: ['window_id'], build: (m, p) => ({ window_id: checks.windowId(m, p.window_id) }) },
  get_screenshot: { kind: 'read', keys: ['window_id'], build: (m, p) => ({ window_id: checks.windowId(m, p.window_id) }) },
  // Starts an installed desktop application by name; not window-scoped.
  launch_app: { kind: 'launch', keys: ['app'], build: (m, p) => ({ app: checks.label(m, 'app', p.app) }) },
  // The display's CLIPBOARD selection (plain text); not window-scoped.
  clipboard_write: { kind: 'clipboard', keys: ['text'], build: (m, p) => ({ text: checks.text(m, 'text', p.text) }) },
  clipboard_read: { kind: 'clipboard', keys: [], build: () => ({}) },
  clipboard_release: { kind: 'clipboard', keys: [], build: () => ({}) },
  click: {
    kind: 'action',
    keys: ['window_id', 'element_index', 'x', 'y', 'button', 'click_count', 'modifiers', 'hold_ms'],
    build(m, p) {
      const out = { window_id: checks.windowId(m, p.window_id) };
      target(m, p, out);
      if (p.button !== undefined) {
        if (!['left', 'right', 'middle'].includes(p.button)) fail(m, 'button must be left, right or middle');
        out.button = p.button;
      }
      if (p.click_count !== undefined) {
        if (![1, 2, 3].includes(p.click_count)) fail(m, 'click_count must be 1, 2 or 3');
        out.click_count = p.click_count;
      }
      if (p.modifiers !== undefined) out.modifiers = checks.modifiers(m, p.modifiers);
      if (p.hold_ms !== undefined) out.hold_ms = checks.holdMs(m, p.hold_ms);
      return out;
    },
  },
  drag: {
    kind: 'action',
    keys: ['window_id', 'from', 'to', 'path', 'modifiers'],
    build(m, p) {
      const out = { window_id: checks.windowId(m, p.window_id) };
      if (p.path !== undefined) {
        if (p.from !== undefined || p.to !== undefined) fail(m, 'provide path or from/to, not both');
        if (!Array.isArray(p.path) || p.path.length < 2 || p.path.length > 256) fail(m, 'path must contain 2..256 [x, y] points');
        out.path = p.path.map((v) => checks.point(m, 'path point', v));
      } else {
        out.from = checks.point(m, 'from', p.from);
        out.to = checks.point(m, 'to', p.to);
      }
      if (p.modifiers !== undefined) out.modifiers = checks.modifiers(m, p.modifiers);
      return out;
    },
  },
  scroll: {
    kind: 'action',
    keys: ['window_id', 'element_index', 'x', 'y', 'direction', 'pixels', 'modifiers'],
    build(m, p) {
      const out = { window_id: checks.windowId(m, p.window_id) };
      target(m, p, out);
      if (!['up', 'down', 'left', 'right'].includes(p.direction)) fail(m, 'direction must be up, down, left or right');
      out.direction = p.direction;
      if (!Number.isInteger(p.pixels) || p.pixels < 1 || p.pixels > 100_000) fail(m, 'pixels must be an integer in [1, 100000]');
      out.pixels = p.pixels;
      if (p.modifiers !== undefined) out.modifiers = checks.modifiers(m, p.modifiers);
      return out;
    },
  },
  type_text: { kind: 'action', keys: ['window_id', 'text'], build: (m, p) => ({ window_id: checks.windowId(m, p.window_id), text: checks.text(m, 'text', p.text) }) },
  press_key: {
    kind: 'action',
    keys: ['window_id', 'key', 'hold_ms'],
    build(m, p) {
      const out = { window_id: checks.windowId(m, p.window_id), key: checks.key(m, p.key) };
      if (p.hold_ms !== undefined) out.hold_ms = checks.holdMs(m, p.hold_ms);
      return out;
    },
  },
  paste: { kind: 'action', keys: ['window_id', 'text'], build: (m, p) => ({ window_id: checks.windowId(m, p.window_id), text: checks.text(m, 'text', p.text) }) },
  set_value: {
    kind: 'action',
    keys: ['window_id', 'element_index', 'value'],
    build: (m, p) => ({ window_id: checks.windowId(m, p.window_id), element_index: checks.elementIndex(m, p.element_index), value: checks.text(m, 'value', p.value, { allowEmpty: true }) }),
  },
  perform_secondary_action: {
    kind: 'action',
    keys: ['window_id', 'element_index', 'action'],
    build: (m, p) => ({ window_id: checks.windowId(m, p.window_id), element_index: checks.elementIndex(m, p.element_index), action: checks.label(m, 'action', p.action) }),
  },
  // Hover: move the virtual pointer to an element, a point, or by an offset.
  mouse_move: {
    kind: 'action',
    keys: ['window_id', 'element_index', 'x', 'y', 'dx', 'dy'],
    build(m, p) {
      const out = { window_id: checks.windowId(m, p.window_id) };
      const relative = p.dx !== undefined || p.dy !== undefined;
      if (relative) {
        if (p.element_index !== undefined || p.x !== undefined || p.y !== undefined) fail(m, 'provide exactly one of element_index, x,y or dx,dy');
        for (const k of ['dx', 'dy']) {
          if (!Number.isInteger(p[k]) || Math.abs(p[k]) > MAX_COORD) fail(m, `${k} must be an integer in [-${MAX_COORD}, ${MAX_COORD}]`);
          out[k] = p[k];
        }
      } else {
        target(m, p, out);
      }
      return out;
    },
  },
};

export function parseExtensions(value) {
  const set = new Set();
  if (!value) return set;
  for (const raw of String(value).split(',')) {
    const name = raw.trim();
    if (!name) continue;
    if (!KNOWN_EXTENSIONS.includes(name)) throw new CuaError('invalid_config', `unknown engine extension: ${name}`);
    set.add(name);
  }
  return set;
}

// Methods model code and discrete tools may call, given enabled extensions.
// `app: true` (engine): window_id names any window of an app, and the engine
// acts on the app's window the input belongs to (dialogs included).
const APP_SCOPED = ['get_app_state', 'get_ax_state', 'get_screenshot', 'click', 'drag', 'scroll', 'type_text', 'press_key', 'paste', 'set_value', 'perform_secondary_action', 'mouse_move'];
for (const m of APP_SCOPED) {
  const spec = METHODS[m];
  const build = spec.build;
  const pointer = ['click', 'drag', 'scroll', 'type_text', 'paste', 'mouse_move'].includes(m);
  const action = spec.kind === 'action';
  const observes = action || ['get_app_state', 'get_ax_state'].includes(m);
  spec.keys = [...spec.keys, 'app', ...(pointer ? ['frame_window'] : []), ...(action ? ['observe'] : []), ...(observes ? ['screenshot'] : [])];
  spec.build = (method, p) => {
    const { app, frame_window, observe, screenshot, ...rest } = p;
    const out = build(method, rest);
    // One request for the observation and a screenshot of its window.
    if (screenshot !== undefined) out.screenshot = checks.bool(method, 'screenshot', screenshot);
    if (app !== undefined) {
      if (typeof app !== 'boolean') fail(method, 'app must be a boolean');
      out.app = app;
    }
    // Set by the sandbox host, not model code: the window of the last state
    // the model received for this app, which app-scoped coordinates refer to.
    if (frame_window !== undefined) {
      if (!pointer || app !== true) fail(method, 'frame_window needs app: true and a pointer or text method');
      out.frame_window = checks.windowId(method, frame_window);
    }
    // Set by the host for an action whose post-action tree nobody reads (the
    // sandbox observes once at the end of a cell; adapters take a screenshot).
    if (observe !== undefined) {
      if (typeof observe !== 'boolean') fail(method, 'observe must be a boolean');
      out.observe = observe;
    }
    return out;
  };
}

// Methods model code and discrete tools may call. (`extensions` is accepted
// for compatibility; every method is available.)
export function exposedMethods() {
  return Object.keys(METHODS);
}

export function methodKind(method) {
  return METHODS[method]?.kind;
}

// Validate a model-originated request; returns a fresh normalized params object.
export function validateRequest(method, params) {
  if (typeof method !== 'string' || !Object.hasOwn(METHODS, method)) {
    if (HOST_ONLY_METHODS.includes(method)) throw new CuaError('forbidden_method', `${method} is reserved for the trusted host`);
    throw new CuaError('unknown_method', `unknown method: ${String(method).slice(0, 64)}`);
  }
  const spec = METHODS[method];
  if (params === undefined || params === null) params = {};
  if (!isPlainObject(params)) throw new CuaError('invalid_params', `${method}: params must be a plain object`);
  const allowed = new Set(spec.keys);
  for (const k of Object.keys(params)) {
    if (!allowed.has(k)) throw new CuaError('invalid_params', `${method}: unknown parameter "${String(k).slice(0, 64)}"`);
  }
  return spec.build(method, params);
}

// JSON Schemas for the discrete MCP tools, derived from the same table.
export function toolInputSchema(method) {
  const props = {};
  const req = [];
  const spec = METHODS[method];
  const add = (k, schema, required = false) => {
    props[k] = schema;
    if (required) req.push(k);
  };
  const int = (min, max, description) => ({ type: 'integer', minimum: min, maximum: max, ...(description ? { description } : {}) });
  const point = { type: 'array', items: int(0, MAX_COORD), minItems: 2, maxItems: 2 };
  for (const k of spec.keys) {
    switch (k) {
      case 'window_id': add(k, int(0, 0xffffffff, 'Window id from list_windows'), true); break;
      case 'element_index': add(k, int(0, MAX_ELEMENT_INDEX, 'Element index from the accessibility tree'), method === 'set_value' || method === 'perform_secondary_action'); break;
      case 'x': case 'y': add(k, int(0, MAX_COORD, 'Window-local pixel coordinate')); break;
      case 'dx': case 'dy': add(k, int(-MAX_COORD, MAX_COORD, 'Offset from the current pointer position (instead of x,y)')); break;
      case 'from': case 'to': add(k, point); break;
      case 'path': add(k, { type: 'array', items: point, minItems: 2, maxItems: 256, description: 'Continuous stroke through every point with the left button held; use instead of from/to' }); break;
      case 'direction': add(k, { type: 'string', enum: ['up', 'down', 'left', 'right'] }, true); break;
      case 'pixels': add(k, int(1, 100_000), true); break;
      case 'text': add(k, { type: 'string', minLength: 1, maxLength: MAX_TEXT }, true); break;
      case 'key': add(k, { type: 'string', description: 'X keysym or chord, e.g. "Return", "ctrl+s"' }, true); break;
      case 'value': add(k, { type: 'string', maxLength: MAX_TEXT }, true); break;
      case 'action': add(k, { type: 'string', description: 'Secondary accessibility action name' }, true); break;
      case 'button': add(k, { type: 'string', enum: ['left', 'right', 'middle'] }); break;
      case 'click_count': add(k, { type: 'integer', enum: [1, 2, 3] }); break;
      case 'modifiers': add(k, { type: 'array', items: { type: 'string', enum: MODIFIERS }, maxItems: 4, description: 'Modifier keys held during the action' }); break;
      case 'hold_ms': add(k, int(0, MAX_HOLD_MS, 'How long the button or key stays pressed')); break;
      case 'screenshot': add(k, { type: 'boolean', description: 'Also return a screenshot of the observed window' }); break;
      case 'app':
        if (method === 'launch_app') add(k, { type: 'string', maxLength: 256, description: 'Desktop entry ID (org.gnome.Calculator), its last part, the menu name or the program name' }, true);
        else add(k, { type: 'boolean', description: 'Treat window_id as the app: act on its focused or dialog window (element: the window holding it)' });
        break;
      case 'frame_window': case 'observe': break; // set by the host only
      default: throw new Error(`no schema for ${k}`);
    }
  }
  return { type: 'object', properties: props, required: req, additionalProperties: false };
}

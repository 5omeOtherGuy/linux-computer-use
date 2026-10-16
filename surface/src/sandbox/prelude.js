// Source evaluated once inside every fresh QuickJS context. It captures the
// two native bridges, removes them from the global object and builds the
// model-facing API. Everything here runs inside the VM: it has no filesystem,
// process, network or module loader, and every host call is revalidated by
// the trusted host.
export function preludeSource() {
  return `(() => {
  'use strict';
  const hostCall = globalThis.__cua_host_call;
  const emit = globalThis.__cua_emit;
  const nativeSleep = globalThis.__cua_sleep;
  delete globalThis.__cua_host_call;
  delete globalThis.__cua_emit;
  delete globalThis.__cua_sleep;

  const fmt = (value) => {
    if (value === undefined) return 'undefined';
    if (typeof value === 'string') return value;
    if (typeof value === 'function') return '[Function ' + (value.name || 'anonymous') + ']';
    if (typeof value === 'bigint') return value.toString() + 'n';
    if (typeof value === 'symbol') return value.toString();
    if (value instanceof Error) return (value.name || 'Error') + ': ' + value.message + (value.code ? ' [' + value.code + ']' : '');
    if (value instanceof ArrayBuffer) return 'ArrayBuffer(' + value.byteLength + ')';
    if (ArrayBuffer.isView(value)) {
      const b = value;
      const kind = b instanceof Uint8Array && b[0] === 0x89 && b[1] === 0x50 ? ' [PNG image; pass it to nodeRepl.emitImage() to show it]' : '';
      return (b.constructor && b.constructor.name || 'TypedArray') + '(' + b.length + ')' + kind;
    }
    const seen = new WeakSet();
    try {
      return JSON.stringify(value, (k, v) => {
        if (typeof v === 'bigint') return v.toString() + 'n';
        if (typeof v === 'function') return '[Function ' + (v.name || 'anonymous') + ']';
        if (v === undefined) return '[undefined]';
        if (v && typeof v === 'object') {
          if (ArrayBuffer.isView(v) || v instanceof ArrayBuffer) return fmt(v);
          if (seen.has(v)) return '[Circular]';
          seen.add(v);
        }
        return v;
      }, 2);
    } catch (e) {
      return String(value);
    }
  };
  Object.defineProperty(globalThis, '__cua_fmt', { value: fmt, writable: false, enumerable: false, configurable: false });

  const fmtError = (e) => {
    if (e && typeof e === 'object') {
      const head = (e.name || 'Error') + ': ' + e.message + (e.code ? ' [' + e.code + ']' : '');
      return e.stack ? head + '\\n' + String(e.stack) : head;
    }
    return 'Uncaught ' + fmt(e);
  };
  Object.defineProperty(globalThis, '__cua_fmt_error', { value: fmtError, writable: false, enumerable: false, configurable: false });

  class CuaError extends Error {
    constructor(code, message, data) {
      super(message);
      this.name = 'CuaError';
      this.code = code;
      if (data !== undefined) this.data = data;
    }
  }

  // view: how the trusted runner presents the result to the VM and to the
  // model ('windows', 'apps', 'state', 'getApp', 'ax', 'screenshot', 'action').
  // Image results arrive as {json, bytes}: bytes decoded natively by the host.
  const call = async (method, params, view, opts) => {
    let payload;
    try {
      payload = JSON.stringify(params === undefined ? {} : params);
    } catch (e) {
      throw new CuaError('invalid_params', method + ': params are not JSON-serializable');
    }
    const raw = await hostCall(method, payload === undefined ? '{}' : payload, JSON.stringify(Object.assign({ view: view || 'json' }, opts || {})));
    const res = JSON.parse(typeof raw === 'string' ? raw : raw.json);
    if (!res.ok) throw new CuaError(res.error.code, res.error.message, res.error.data);
    if (typeof raw !== 'string' && res.value && res.value.$bytes === true) {
      const bytes = new Uint8Array(raw.bytes);
      hostImages.set(bytes, res.value.$image);
      return bytes;
    }
    return res.value;
  };

  // --- image bytes -------------------------------------------------------
  // Screenshot bytes the host still holds, so emitting them needs no re-encode.
  const hostImages = new WeakMap();
  const sniff = (b) => b[0] === 0x89 && b[1] === 0x50 ? 'image/png' : b[0] === 0xff && b[1] === 0xd8 ? 'image/jpeg' : null;
  const emitImage = async (image) => {
    let bytes = image, mimeType;
    if (typeof image === 'string') {
      const m = /^data:(image\\/(?:png|jpeg));base64,([A-Za-z0-9+/=]+)$/.exec(image);
      if (!m) throw new CuaError('invalid_params', 'emitImage(): a string must be a data:image/png or image/jpeg base64 URL');
      emit('image', JSON.stringify({ data: m[2], mimeType: m[1] }));
      return;
    }
    if (image && !(image instanceof Uint8Array) && typeof image === 'object' && !(image instanceof ArrayBuffer)) {
      if (image.screenshot !== undefined && image.bytes === undefined) return emitImage(image.screenshot);
      bytes = image.bytes; mimeType = image.mimeType;
    }
    if (bytes instanceof ArrayBuffer) bytes = new Uint8Array(bytes);
    if (!(bytes instanceof Uint8Array)) throw new CuaError('invalid_params', 'emitImage() expects image bytes (Uint8Array), {bytes, mimeType} or a data: URL');
    const id = hostImages.get(bytes);
    if (id !== undefined) { emit('image', JSON.stringify({ id })); return; }
    mimeType = mimeType || sniff(bytes);
    if (mimeType !== 'image/png' && mimeType !== 'image/jpeg') throw new CuaError('invalid_params', 'emitImage(): only PNG and JPEG images are supported');
    emit('image', JSON.stringify({ mimeType }), bytes.slice().buffer);
  };

  const fmtText = (v) => typeof v === 'string' ? v : fmt(v);
  const write = (value) => { emit('text', JSON.stringify({ text: fmtText(value) })); };
  const nodeRepl = Object.freeze({ write, emitImage });

  // --- cua API (OpenAI-compatible names; GPT- and Claude-style argument shapes) ---
  const options = (o, allowed, what) => {
    if (o === undefined || o === null) return {};
    if (typeof o !== 'object') throw new CuaError('invalid_params', what + ': options must be an object');
    for (const k of Object.keys(o)) if (!allowed.includes(k)) throw new CuaError('unsupported', what + ': unsupported option ' + k + ' (supported: ' + (allowed.join(', ') || 'none') + ')');
    return o;
  };
  const emitOpt = (o) => ({ emit: o.emit !== false });
  const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const ELEMENT_KEYS = ['element', 'elementId', 'elementIndex', 'element_index', 'index'];
  const elementOf = (o) => { for (const k of ELEMENT_KEYS) if (o[k] !== undefined) return o[k]; return undefined; };
  const pointOf = (o) => (o.x !== undefined || o.y !== undefined) ? [o.x, o.y] : undefined;
  // Element ID, [x, y], {x, y} or {element}.
  const at = (t, what) => {
    if (Number.isInteger(t)) return { element_index: t };
    if (typeof t === 'string' && /^\\d+$/.test(t)) return { element_index: Number(t) };
    if (Array.isArray(t) && t.length === 2) return { x: t[0], y: t[1] };
    if (isObj(t)) {
      const e = elementOf(t);
      if (e !== undefined) return at(e, what);
      const p = pointOf(t);
      if (p) return { x: p[0], y: p[1] };
    }
    throw new CuaError('invalid_params', what + ': target must be an element ID, [x, y], {x, y} or {element}');
  };
  const pick = (o, keys) => { const out = {}; for (const k of keys) if (o[k] !== undefined) out[k] = o[k]; return out; };
  const keyName = (k) => {
    if (Array.isArray(k)) k = k.join('+');
    if (typeof k !== 'string') throw new CuaError('invalid_params', 'pressKey: key must be a string such as "Return" or "ctrl+s", or a list of keys');
    return k.trim().replace(/ *\\+ */g, '+');
  };
  const SCROLL_DIRS = ['up', 'down', 'left', 'right'];

  const cua = Object.create(null);
  cua.CuaError = CuaError;
  cua.listWindows = (o) => call('list_windows', {}, 'windows', emitOpt(options(o, ['emit'], 'listWindows')));
  cua.listApps = (o) => call('list_windows', {}, 'apps', emitOpt(options(o, ['emit'], 'listApps')));
  cua.getState = (o) => call('list_windows', {}, 'state', emitOpt(options(o, ['emit'], 'getState')));
  // getApp("xed"), getApp({appId}), getApp({windowId}) or getApp(windowInfo).
  cua.getApp = async (target, o) => {
    o = options(o, ['emit'], 'getApp');
    let q;
    if (typeof target === 'string') q = { appId: target };
    else if (Number.isInteger(target)) q = { windowId: target };
    else if (isObj(target)) {
      if (target.windowId !== undefined) q = { windowId: target.windowId };
      else if (target.appId !== undefined) q = { appId: target.appId };
      else if (Number.isInteger(target.id) && target.app !== undefined) q = { windowId: target.id };
      else if (typeof target.id === 'string') q = { appId: target.id };
    }
    if (!q) throw new CuaError('invalid_params', 'getApp requires an app id ("xed"), {appId}, or {windowId} (a WindowInfo id)');
    const info = await call('list_windows', q, 'getApp', emitOpt(o));
    const window_id = info.window.id;
    const act = (method, params) => call(method, Object.assign({ window_id, app: true }, params), 'action').then(() => undefined);
    const obs = (method, view, opts) => call(method, { window_id, app: true }, view, opts);
    const app = Object.freeze({
      id: info.app.id,
      displayName: info.app.displayName,
      isRunning: true,
      windows: Object.freeze(info.app.windows),
      window: info.window,
      windowId: window_id,
      // The app's focused (or modal dialog) window first, then a line listing its other windows.
      getAXState(o = {}) {
        o = options(o, ['emit', 'disableDiffing', 'query'], 'getAXState');
        if (o.query !== undefined && typeof o.query !== 'string') throw new CuaError('invalid_params', 'getAXState: query must be a string');
        return obs('get_ax_state', 'ax', { emit: o.emit !== false, disableDiffing: !!o.disableDiffing, query: o.query });
      },
      getScreenshot(o = {}) {
        o = options(o, ['emit'], 'getScreenshot');
        return obs('get_screenshot', 'screenshot', emitOpt(o));
      },
      async getAXStateAndScreenshot(o = {}) {
        o = options(o, ['emit', 'disableDiffing', 'query'], 'getAXStateAndScreenshot');
        const state = await app.getAXState(o);
        const screenshot = await obs('get_screenshot', 'screenshot', emitOpt(o));
        return { state, screenshot };
      },
      // click(id | [x, y] | {x, y} | {element}, {mouseButton, clickCount})
      click(target, o = {}) {
        if (isObj(target) && isObj(o) && Object.keys(o).length === 0) o = pick(target, ['mouseButton', 'button', 'clickCount']);
        o = options(o, ['mouseButton', 'button', 'clickCount'], 'click');
        const p = at(target, 'click');
        const button = o.mouseButton !== undefined ? o.mouseButton : o.button;
        if (button !== undefined && button !== 'left') p.button = button;
        if (o.clickCount !== undefined && o.clickCount !== 1) p.click_count = o.clickCount;
        return act('click', p);
      },
      doubleClick(target) { return app.click(target, { clickCount: 2 }); },
      // drag([x, y], [x, y]) or drag({from, to}) / drag({path: [{x, y}, ...]})
      drag(from, to) {
        if (isObj(from) && to === undefined) {
          if (Array.isArray(from.path) && from.path.length >= 2) { to = from.path[from.path.length - 1]; from = from.path[0]; }
          else { to = from.to; from = from.from; }
        }
        const pt = (v) => Array.isArray(v) ? v : isObj(v) ? [v.x, v.y] : v;
        return act('drag', { from: pt(from), to: pt(to) });
      },
      // scroll(target, direction, distance = 1 page-ish unit) or
      // scroll({x, y | element, direction, distance | pixels | scrollX/scrollY})
      scroll(target, direction, distance = 1) {
        let p, pixels;
        if (isObj(target) && direction === undefined) {
          const o = target;
          p = at(elementOf(o) !== undefined ? { element: elementOf(o) } : o, 'scroll');
          direction = o.direction;
          const sx = o.scrollX ?? o.scroll_x ?? 0, sy = o.scrollY ?? o.scroll_y ?? 0;
          if (direction === undefined && (sx || sy)) {
            direction = Math.abs(sy) >= Math.abs(sx) ? (sy > 0 ? 'down' : 'up') : (sx > 0 ? 'right' : 'left');
            pixels = Math.max(Math.abs(sx), Math.abs(sy));
          }
          if (o.pixels !== undefined) pixels = o.pixels;
          else if (pixels === undefined) distance = o.distance ?? o.amount ?? 1;
        } else {
          p = at(target, 'scroll');
          if (isObj(distance)) pixels = distance.pixels;
        }
        if (typeof direction === 'string') direction = direction.toLowerCase();
        if (!SCROLL_DIRS.includes(direction)) throw new CuaError('invalid_params', 'scroll: direction must be up, down, left or right');
        if (pixels === undefined) pixels = Math.round(Number(distance) * 500);
        return act('scroll', Object.assign(p, { direction, pixels: Math.max(1, Math.round(pixels)) }));
      },
      typeText(text) {
        if (isObj(text)) text = text.text;
        if (typeof text !== 'string') throw new CuaError('invalid_params', 'typeText(text) expects a string');
        return act('type_text', { text });
      },
      // pressKey("ctrl+s"), pressKey(["ctrl", "s"]), pressKey({key}) or pressKey({keys})
      pressKey(key, o = {}) {
        if (isObj(key)) key = key.key ?? key.keys;
        options(o, [], 'pressKey');
        return act('press_key', { key: keyName(key) });
      },
      paste(text, o = {}) {
        if (isObj(text)) { o = pick(text, ['format']); text = text.text; }
        o = options(o, ['format'], 'paste');
        if (o.format && o.format !== 'text') throw new CuaError('unsupported', 'Only plain-text paste is supported');
        return act('paste', { text });
      },
      // setValue(element, value) or setValue({element, value})
      setValue(element, value) {
        if (isObj(element) && value === undefined) { value = element.value; element = elementOf(element); }
        return act('set_value', { element_index: element, value: typeof value === 'number' || typeof value === 'boolean' ? String(value) : value });
      },
      performSecondaryAction(element, action) {
        if (isObj(element) && action === undefined) { action = element.action ?? element.name; element = elementOf(element); }
        return act('perform_secondary_action', { element_index: element, action });
      },
    });
    return app;
  };
  Object.freeze(cua);

  const log = (level) => (...args) => emit('text', JSON.stringify({ level, text: args.map(fmtText).join(' ') }));
  const console = Object.freeze({ log: log('log'), info: log('info'), warn: log('warn'), error: log('error'), debug: log('debug') });
  const display = (image) => emitImage(image);

  const sleep = (ms) => {
    if (typeof ms !== 'number' || !(ms >= 0)) throw new CuaError('invalid_params', 'sleep(ms) expects a non-negative number');
    return nativeSleep(ms);
  };

  for (const [name, value] of Object.entries({ cua, nodeRepl, console, display, sleep })) {
    Object.defineProperty(globalThis, name, { value, writable: false, enumerable: false, configurable: false });
  }
})();`;
}

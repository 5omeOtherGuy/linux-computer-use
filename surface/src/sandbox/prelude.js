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
    const seen = new WeakSet();
    try {
      return JSON.stringify(value, (k, v) => {
        if (typeof v === 'bigint') return v.toString() + 'n';
        if (typeof v === 'function') return '[Function ' + (v.name || 'anonymous') + ']';
        if (v === undefined) return '[undefined]';
        if (v && typeof v === 'object') {
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
  // model ('json', 'ax', 'screenshot', 'axAndScreenshot', 'action').
  const call = async (method, params, view, opts) => {
    let payload;
    try {
      payload = JSON.stringify(params === undefined ? {} : params);
    } catch (e) {
      throw new CuaError('invalid_params', method + ': params are not JSON-serializable');
    }
    const raw = await hostCall(method, payload === undefined ? '{}' : payload, JSON.stringify(Object.assign({ view: view || 'json' }, opts || {})));
    const res = JSON.parse(raw);
    if (res.ok) return res.value;
    throw new CuaError(res.error.code, res.error.message, res.error.data);
  };

  // --- image bytes -------------------------------------------------------
  const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  const B64I = new Uint8Array(128);
  for (let i = 0; i < 64; i++) B64I[B64.charCodeAt(i)] = i;
  const fromBase64 = (s) => {
    const clean = s.replace(/=+$/, '');
    const out = new Uint8Array(Math.floor(clean.length * 3 / 4));
    let o = 0;
    for (let i = 0; i < clean.length; i += 4) {
      const n = (B64I[clean.charCodeAt(i)] << 18) | (B64I[clean.charCodeAt(i + 1)] << 12)
        | ((B64I[clean.charCodeAt(i + 2)] || 0) << 6) | (B64I[clean.charCodeAt(i + 3)] || 0);
      if (o < out.length) out[o++] = (n >> 16) & 255;
      if (o < out.length) out[o++] = (n >> 8) & 255;
      if (o < out.length) out[o++] = n & 255;
    }
    return out;
  };
  const toBase64 = (bytes) => {
    const parts = [];
    for (let i = 0; i < bytes.length; i += 3) {
      const n = (bytes[i] << 16) | ((bytes[i + 1] || 0) << 8) | (bytes[i + 2] || 0);
      parts.push(B64[(n >> 18) & 63], B64[(n >> 12) & 63], i + 1 < bytes.length ? B64[(n >> 6) & 63] : '=', i + 2 < bytes.length ? B64[n & 63] : '=');
    }
    return parts.join('');
  };
  // Screenshot bytes the host still holds, so emitting them needs no re-encode.
  const hostImages = new WeakMap();
  const toBytes = (img) => {
    if (!img || typeof img.$bytes !== 'string') return img;
    const bytes = fromBase64(img.$bytes);
    hostImages.set(bytes, img.$image);
    return bytes;
  };
  const sniff = (b) => b[0] === 0x89 && b[1] === 0x50 ? 'image/png' : b[0] === 0xff && b[1] === 0xd8 ? 'image/jpeg' : null;
  const emitImage = async (image) => {
    let bytes = image, mimeType;
    if (typeof image === 'string') {
      const m = /^data:(image\\/(?:png|jpeg));base64,([A-Za-z0-9+/=]+)$/.exec(image);
      if (!m) throw new CuaError('invalid_params', 'emitImage(): a string must be a data:image/png or image/jpeg base64 URL');
      emit('image', JSON.stringify({ data: m[2], mimeType: m[1] }));
      return;
    }
    if (image && !(image instanceof Uint8Array) && typeof image === 'object') {
      if (image.screenshot !== undefined && image.bytes === undefined) return emitImage(image.screenshot);
      bytes = image.bytes; mimeType = image.mimeType;
    }
    if (bytes instanceof ArrayBuffer) bytes = new Uint8Array(bytes);
    if (!(bytes instanceof Uint8Array)) throw new CuaError('invalid_params', 'emitImage() expects image bytes (Uint8Array), {bytes, mimeType} or a data: URL');
    const id = hostImages.get(bytes);
    if (id !== undefined) { emit('image', JSON.stringify({ id })); return; }
    mimeType = mimeType || sniff(bytes);
    if (mimeType !== 'image/png' && mimeType !== 'image/jpeg') throw new CuaError('invalid_params', 'emitImage(): only PNG and JPEG images are supported');
    emit('image', JSON.stringify({ data: toBase64(bytes), mimeType }));
  };

  const fmtText = (v) => typeof v === 'string' ? v : fmt(v);
  const write = (value) => { emit('text', JSON.stringify({ text: fmtText(value) })); };
  const nodeRepl = Object.freeze({ write, emitImage });

  // --- cua API (OpenAI-compatible names) -----------------------------------
  const options = (o, allowed, what) => {
    if (o === undefined || o === null) return {};
    if (typeof o !== 'object') throw new CuaError('invalid_params', what + ': options must be an object');
    for (const k of Object.keys(o)) if (!allowed.includes(k)) throw new CuaError('unsupported', what + ': unsupported option ' + k);
    return o;
  };
  const emitOpt = (o) => ({ emit: o.emit !== false });
  const cua = Object.create(null);
  cua.CuaError = CuaError;
  cua.listWindows = (o) => call('list_windows', {}, 'windows', emitOpt(options(o, ['emit'], 'listWindows')));
  cua.listApps = (o) => call('list_windows', {}, 'apps', emitOpt(options(o, ['emit'], 'listApps')));
  cua.getState = (o) => call('list_windows', {}, 'state', emitOpt(options(o, ['emit'], 'getState')));
  cua.getApp = async (target, o) => {
    o = options(o, ['emit'], 'getApp');
    if (!target || typeof target !== 'object' || (target.windowId === undefined) === (target.appId === undefined)) {
      throw new CuaError('invalid_params', 'getApp requires {windowId} (a WindowInfo id) or {appId}');
    }
    const info = await call('list_windows', target.windowId !== undefined ? { windowId: target.windowId } : { appId: target.appId }, 'getApp', emitOpt(o));
    const window_id = info.window.id;
    const at = (t) => Number.isInteger(t) ? { element_index: t }
      : Array.isArray(t) && t.length === 2 ? { x: t[0], y: t[1] }
      : (() => { throw new CuaError('invalid_params', 'target must be an element ID or [x, y]'); })();
    const act = (method, params) => call(method, Object.assign({ window_id }, params), 'action').then(() => undefined);
    return Object.freeze({
      id: info.app.id,
      displayName: info.app.displayName,
      isRunning: true,
      windows: Object.freeze([info.window]),
      window: info.window,
      windowId: info.window.id,
      getAXState(o = {}) {
        o = options(o, ['emit', 'disableDiffing'], 'getAXState');
        return call('get_ax_state', { window_id }, 'ax', { emit: o.emit !== false, disableDiffing: !!o.disableDiffing });
      },
      async getScreenshot(o = {}) {
        o = options(o, ['emit'], 'getScreenshot');
        return toBytes(await call('get_screenshot', { window_id }, 'screenshot', emitOpt(o)));
      },
      async getAXStateAndScreenshot(o = {}) {
        o = options(o, ['emit', 'disableDiffing'], 'getAXStateAndScreenshot');
        const state = await call('get_ax_state', { window_id }, 'ax', { emit: o.emit !== false, disableDiffing: !!o.disableDiffing });
        const screenshot = toBytes(await call('get_screenshot', { window_id }, 'screenshot', emitOpt(o)));
        return { state, screenshot };
      },
      click(target, o = {}) {
        o = options(o, ['mouseButton', 'clickCount'], 'click');
        const p = at(target);
        if (o.mouseButton !== undefined) p.button = o.mouseButton;
        if (o.clickCount !== undefined) p.click_count = o.clickCount;
        return act('click', p);
      },
      drag(from, to) { return act('drag', { from, to }); },
      scroll(target, direction, distance = 1) {
        const pixels = typeof distance === 'object' && distance !== null ? distance.pixels : Math.round(distance * 500);
        return act('scroll', Object.assign(at(target), { direction, pixels }));
      },
      typeText(text) { return act('type_text', { text }); },
      pressKey(key, o = {}) { options(o, [], 'pressKey'); return act('press_key', { key }); },
      paste(text, o = {}) {
        o = options(o, ['format'], 'paste');
        if (o.format && o.format !== 'text') throw new CuaError('unsupported', 'Only plain-text paste is supported');
        return act('paste', { text });
      },
      setValue(element_index, value) { return act('set_value', { element_index, value }); },
      performSecondaryAction(element_index, action) { return act('perform_secondary_action', { element_index, action }); },
    });
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

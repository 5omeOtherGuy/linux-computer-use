// Source evaluated once inside every fresh QuickJS context. It captures the
// two native bridges, removes them from the global object and builds the
// model-facing API. Everything here runs inside the VM: it has no filesystem,
// process, network or module loader, and every host call is revalidated by
// the trusted host.
export function preludeSource(methods) {
  return `(() => {
  'use strict';
  const hostCall = globalThis.__cua_host_call;
  const emit = globalThis.__cua_emit;
  const nativeSleep = globalThis.__cua_sleep;
  delete globalThis.__cua_host_call;
  delete globalThis.__cua_emit;
  delete globalThis.__cua_sleep;
  const METHODS = ${JSON.stringify(methods)};

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

  const call = async (method, params) => {
    let payload;
    try {
      payload = JSON.stringify(params === undefined ? {} : params);
    } catch (e) {
      throw new CuaError('invalid_params', method + ': params are not JSON-serializable');
    }
    const raw = await hostCall(method, payload === undefined ? '{}' : payload);
    const res = JSON.parse(raw);
    if (res.ok) return res.value;
    throw new CuaError(res.error.code, res.error.message, res.error.data);
  };

  const camel = (s) => s.replace(/_([a-z])/g, (_, c) => c.toUpperCase());
  const cua = Object.create(null);
  for (const m of METHODS) {
    const fn = (params) => call(m, params);
    Object.defineProperty(fn, 'name', { value: m });
    cua[m] = fn;
    if (camel(m) !== m) cua[camel(m)] = fn;
  }
  cua.methods = Object.freeze(METHODS.slice());
  cua.CuaError = CuaError;
  cua.computer = Object.freeze({ target: 'linux' });
  cua.getState = async () => ({ apps: await call('list_apps', {}), browsers: [] });
  cua.getApp = async (target) => {
    if (!target || !Number.isInteger(target.windowId)) throw new CuaError('invalid_params', 'getApp requires {windowId}');
    const window_id = target.windowId;
    const at = (target) => Number.isInteger(target) ? { element_index: target }
      : Array.isArray(target) && target.length === 2 ? { x: target[0], y: target[1] }
      : (() => { throw new CuaError('invalid_params', 'target must be an element ID or [x,y]'); })();
    const options = (o, allowed) => {
      for (const k of Object.keys(o || {})) if (!allowed.includes(k)) throw new CuaError('unsupported', 'Unsupported option: ' + k);
    };
    return Object.freeze({
      windowId: window_id,
      async getAXState(o = {}) {
        options(o, ['emit', 'disableDiffing']);
        const state = await call('get_ax_state', { window_id });
        return o.disableDiffing ? state.accessibility : (state.ax_diff || state.accessibility);
      },
      async getScreenshot(o = {}) { options(o, ['emit']); return call('get_screenshot', { window_id }); },
      async getAXStateAndScreenshot(o = {}) {
        options(o, ['emit', 'disableDiffing']);
        return { state: await this.getAXState(o), screenshot: await this.getScreenshot({ emit: o.emit }) };
      },
      click(target, o = {}) {
        options(o, ['mouseButton', 'clickCount']);
        const p = { window_id, ...at(target) };
        if (o.mouseButton !== undefined) p.button = o.mouseButton;
        if (o.clickCount !== undefined) p.click_count = o.clickCount;
        return call('click', p);
      },
      drag(from, to) { return call('drag', { window_id, from, to }); },
      scroll(target, direction, distance = 1) {
        const pixels = typeof distance === 'object' ? distance.pixels : Math.round(distance * 500);
        return call('scroll', { window_id, ...at(target), direction, pixels });
      },
      typeText(text) { return call('type_text', { window_id, text }); },
      pressKey(key, o = {}) { options(o, []); return call('press_key', { window_id, key }); },
      paste(text, o = {}) {
        options(o, ['format']);
        if (o.format && o.format !== 'text') throw new CuaError('unsupported', 'Only plain-text paste is supported');
        return call('paste', { window_id, text });
      },
      setValue(element_index, value) { return call('set_value', { window_id, element_index, value }); },
      performSecondaryAction(element_index, action) { return call('perform_secondary_action', { window_id, element_index, action }); },
    });
  };
  Object.freeze(cua);

  const log = (level) => (...args) => emit('log', JSON.stringify({ level, text: args.map(fmt).join(' ') }));
  const console = Object.freeze({ log: log('log'), info: log('info'), warn: log('warn'), error: log('error'), debug: log('debug') });

  const display = (image) => {
    if (!image || typeof image !== 'object' || typeof image.$image !== 'string') {
      throw new CuaError('invalid_params', 'display() expects an image handle, e.g. (await cua.get_screenshot({window_id})) or observation.screenshot');
    }
    emit('image', JSON.stringify({ id: image.$image }));
  };

  const sleep = (ms) => {
    if (typeof ms !== 'number' || !(ms >= 0)) throw new CuaError('invalid_params', 'sleep(ms) expects a non-negative number');
    return nativeSleep(ms);
  };

  for (const [name, value] of Object.entries({ cua, console, display, sleep })) {
    Object.defineProperty(globalThis, name, { value, writable: false, enumerable: false, configurable: false });
  }
})();`;
}

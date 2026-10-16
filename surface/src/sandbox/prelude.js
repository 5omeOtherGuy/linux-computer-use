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
  const helpText = typeof globalThis.__cua_help === 'string' ? globalThis.__cua_help : '';
  delete globalThis.__cua_host_call;
  delete globalThis.__cua_emit;
  delete globalThis.__cua_sleep;
  delete globalThis.__cua_help;

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
      const png = b instanceof Uint8Array && b[0] === 0x89 && b[1] === 0x50;
      const kind = !png ? '' : shownImages.has(b) ? ' [PNG image, shown in this output]' : ' [PNG image; pass it to nodeRepl.emitImage() to show it]';
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
      if (!opts || opts.emit !== false) shownImages.add(bytes);
      // An observation captured together with its screenshot.
      if (res.value.state !== undefined) return { state: res.value.state, screenshot: bytes };
      return bytes;
    }
    return res.value;
  };

  // --- image bytes -------------------------------------------------------
  // Screenshot bytes the host still holds, so emitting them needs no re-encode.
  const hostImages = new WeakMap();
  // Images already emitted into the cell output (auto-emitted screenshots).
  const shownImages = new WeakSet();
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
  // A locator names an element instead of numbering it: {role, name, text, states}.
  const LOCATOR_KEYS = ['role', 'name', 'text', 'states'];
  const isLocator = (t) => isObj(t) && elementOf(t) === undefined && pointOf(t) === undefined && LOCATOR_KEYS.some((k) => t[k] !== undefined);
  // Element ID, [x, y], {x, y}, {element} or a locator.
  const at = (t, what) => {
    if (Number.isInteger(t)) return { element_index: t };
    if (typeof t === 'string' && /^\\d+$/.test(t)) return { element_index: Number(t) };
    if (Array.isArray(t) && t.length === 2) return { x: t[0], y: t[1] };
    if (isObj(t)) {
      const e = elementOf(t);
      if (e !== undefined) return at(e, what);
      const p = pointOf(t);
      if (p) return { x: p[0], y: p[1] };
      if (isLocator(t)) return { locator: pick(t, LOCATOR_KEYS) };
    }
    throw new CuaError('invalid_params', what + ': target must be an element ID, [x, y], {x, y}, {element} or {role, name}');
  };
  const elementRef = (e) => isLocator(e) ? { locator: pick(e, LOCATOR_KEYS) } : { element_index: e };

  // Role spellings models use for the engine's AT-SPI role names.
  const ROLE_GROUPS = [
    ['button', 'push button', 'pushbutton', 'toggle button', 'push button menu'],
    ['text', 'entry', 'password text', 'textbox', 'text box', 'textfield', 'text field', 'edit', 'input', 'textarea', 'searchbox', 'search box'],
    ['check box', 'checkbox', 'switch'],
    ['radio button', 'radio', 'radiobutton'],
    ['menu item', 'menuitem', 'check menu item', 'radio menu item'],
    ['combo box', 'combobox', 'dropdown', 'select'],
    ['list item', 'listitem', 'option'],
    ['page tab', 'tab'],
    ['table cell', 'cell', 'gridcell'],
    ['tree item', 'treeitem'],
  ];
  const roleKey = (r) => {
    const s = String(r).toLowerCase().replace(/[_-]/g, ' ').replace(/\\s+/g, ' ').trim();
    const g = ROLE_GROUPS.findIndex((grp) => grp.includes(s));
    return g < 0 ? s : 'group' + g;
  };
  const norm = (s) => String(s ?? '').replace(/\\s+/g, ' ').trim().toLowerCase();
  const LOCATOR_FIELDS = (el) => [el.name, el.desc, el.text, el.value];
  // Matching elements, exact name matches before partial ones.
  const matchElements = (elements, loc) => {
    for (const k of Object.keys(loc)) if (!LOCATOR_KEYS.includes(k)) throw new CuaError('invalid_params', 'locator: unknown key ' + k + ' (use role, name, text, states)');
    const test = (pattern, value, exact) => pattern instanceof RegExp
      ? (pattern.lastIndex = 0, pattern.test(String(value ?? '')))
      : exact ? norm(value) === norm(pattern) : norm(value).includes(norm(pattern));
    let found = elements;
    if (loc.role !== undefined) { const r = roleKey(loc.role); found = found.filter((el) => roleKey(el.role) === r); }
    if (loc.states !== undefined) {
      const want = (Array.isArray(loc.states) ? loc.states : [loc.states]).map(norm);
      found = found.filter((el) => want.every((s) => el.states.includes(s)));
    }
    if (loc.text !== undefined) found = found.filter((el) => LOCATOR_FIELDS(el).some((v) => v !== undefined && test(loc.text, v, false)));
    if (loc.name !== undefined) {
      const exact = found.filter((el) => test(loc.name, el.name, true));
      found = exact.length || loc.name instanceof RegExp ? exact : found.filter((el) => test(loc.name, el.name, false));
    }
    return found;
  };
  const describeLocator = (loc) => fmt(Object.fromEntries(Object.entries(loc).map(([k, v]) => [k, v instanceof RegExp ? String(v) : v]))).replace(/\\s+/g, ' ');
  const describeElement = (el) => '[' + el.id + '] ' + el.role + (el.name ? ' ' + JSON.stringify(el.name.slice(0, 60)) : '') + (el.states.length ? ' (' + el.states.join(', ') + ')' : '');
  // Locators wait this long for their element (a dialog still being built).
  const LOCATOR_WAIT_MS = 1000;
  const pick = (o, keys) => { const out = {}; for (const k of keys) if (o[k] !== undefined) out[k] = o[k]; return out; };
  const keyName = (k) => {
    if (Array.isArray(k)) k = k.join('+');
    if (typeof k !== 'string') throw new CuaError('invalid_params', 'pressKey: key must be a string such as "Return" or "ctrl+s", or a list of keys');
    return k.trim().replace(/ *\\+ */g, '+');
  };
  const SCROLL_DIRS = ['up', 'down', 'left', 'right'];
  // Held modifier keys of a pointer action: {modifiers: ["shift", "ctrl"]},
  // {modifiers: "shift+ctrl"}, or the key/keys/holdKeys spellings.
  const MOD_OPTS = ['modifiers', 'holdKeys', 'keys', 'key'];
  const MOD_ALIAS = { control: 'ctrl', ctl: 'ctrl', meta: 'alt', option: 'alt', win: 'super', windows: 'super', logo: 'super' };
  const modsOf = (o, what) => {
    let m = o.modifiers ?? o.holdKeys ?? o.keys ?? o.key;
    if (m === undefined) return undefined;
    if (typeof m === 'string') m = m.split('+');
    if (!Array.isArray(m)) throw new CuaError('invalid_params', what + ': modifiers must be "shift", "ctrl+shift" or an array of modifier names');
    return m.map((s) => { s = String(s).trim().toLowerCase(); return MOD_ALIAS[s] || s; });
  };
  // How long a button or key stays pressed: {durationMs} (or holdMs).
  const HOLD_OPTS = ['durationMs', 'holdMs', 'duration_ms', 'hold_ms'];
  const holdOf = (o) => o.durationMs ?? o.holdMs ?? o.duration_ms ?? o.hold_ms;
  const CLICK_OPTS = ['mouseButton', 'button', 'clickCount', ...MOD_OPTS, ...HOLD_OPTS];

  // An App object for a window the host resolved ({window, app}).
  const makeApp = (info) => {
    const window_id = info.window.id;
    // Matches of a locator in the app's current window, waiting up to waitMs
    // for at least one.
    const findAll = async (loc, waitMs) => {
      const until = Date.now() + waitMs;
      for (;;) {
        const r = await call('get_ax_state', { window_id, app: true }, 'elements');
        const found = matchElements(r.elements, loc);
        if (found.length || Date.now() >= until) return { found, partial: r.partial };
        await nativeSleep(150);
      }
    };
    const resolve = async (loc, what) => {
      const { found, partial } = await findAll(loc, LOCATOR_WAIT_MS);
      if (found.length === 1) return found[0].id;
      if (found.length === 0) {
        throw new CuaError('element_not_found', what + ': no element matches ' + describeLocator(loc) + ' in the app\\'s active window' + (partial ? ' (the observation was partial)' : ''));
      }
      throw new CuaError('ambiguous_element', what + ': ' + found.length + ' elements match ' + describeLocator(loc) + ': ' + found.slice(0, 8).map(describeElement).join('; ') + (found.length > 8 ? '; …' : '') + '. Add role, name or text, or use an ID');
    };
    const act = async (method, params) => {
      if (params.locator !== undefined) {
        const { locator, ...rest } = params;
        params = Object.assign(rest, { element_index: await resolve(locator, method) });
      }
      await call(method, Object.assign({ window_id, app: true }, params), 'action');
    };
    const obs = (method, view, opts) => call(method, { window_id, app: true }, view, opts);
    const axOptions = (o, what) => {
      o = options(o, ['emit', 'disableDiffing', 'query'], what);
      if (o.query !== undefined && typeof o.query !== 'string') throw new CuaError('invalid_params', what + ': query must be a string');
      return { emit: o.emit !== false, disableDiffing: !!o.disableDiffing, query: o.query };
    };
    const app = Object.freeze({
      id: info.app.id,
      displayName: info.app.displayName,
      isRunning: true,
      windows: Object.freeze(info.app.windows),
      window: info.window,
      windowId: window_id,
      // The process launchApp started (absent for getApp).
      ...(Number.isInteger(info.pid) ? { pid: info.pid } : {}),
      // The app's focused (or modal dialog) window first, then a line listing its other windows.
      getAXState(o = {}) {
        return obs('get_ax_state', 'ax', axOptions(o, 'getAXState'));
      },
      getScreenshot(o = {}) {
        o = options(o, ['emit'], 'getScreenshot');
        return obs('get_screenshot', 'screenshot', emitOpt(o));
      },
      // find({role, name, text, states}): matching elements of the app's active
      // window as [{id, role, name, states, ...}], without showing them.
      async find(loc, o = {}) {
        if (!isObj(loc) || !LOCATOR_KEYS.some((k) => loc[k] !== undefined)) throw new CuaError('invalid_params', 'find expects a locator such as {role: "button", name: "Save"}');
        o = options(o, ['timeoutMs'], 'find');
        return (await findAll(loc, Math.max(0, Number(o.timeoutMs) || 0))).found;
      },
      // One engine request for the tree and the screenshot: {state, screenshot}.
      getAXStateAndScreenshot(o = {}) {
        return call('get_ax_state', { window_id, app: true, screenshot: true }, 'ax', axOptions(o, 'getAXStateAndScreenshot'));
      },
      // click(id | [x, y] | {x, y} | {element}, {mouseButton, clickCount, modifiers, durationMs})
      click(target, o = {}) {
        if (isObj(target) && isObj(o) && Object.keys(o).length === 0) o = pick(target, CLICK_OPTS);
        o = options(o, CLICK_OPTS, 'click');
        const p = at(target, 'click');
        const button = o.mouseButton !== undefined ? o.mouseButton : o.button;
        if (button !== undefined && button !== 'left') p.button = button;
        if (o.clickCount !== undefined && o.clickCount !== 1) p.click_count = o.clickCount;
        const mods = modsOf(o, 'click');
        if (mods) p.modifiers = mods;
        const hold = holdOf(o);
        if (hold !== undefined) p.hold_ms = hold;
        return act('click', p);
      },
      doubleClick(target, o) { return app.click(target, Object.assign({}, o || {}, { clickCount: 2 })); },
      rightClick(target, o) { return app.click(target, Object.assign({}, o || {}, { mouseButton: 'right' })); },
      // Hover: moveMouse(id | [x, y] | {x, y}) or moveMouse({dx, dy}); tooltips need ~0.5 s before a screenshot.
      moveMouse(target) {
        if (isObj(target) && (target.dx !== undefined || target.dy !== undefined) && elementOf(target) === undefined && pointOf(target) === undefined) {
          return act('mouse_move', { dx: target.dx ?? 0, dy: target.dy ?? 0 });
        }
        return act('mouse_move', at(target, 'moveMouse'));
      },
      moveMouseRelative(dx, dy) {
        if (isObj(dx)) { dy = dx.dy; dx = dx.dx; }
        return act('mouse_move', { dx, dy });
      },
      hover(target) { return app.moveMouse(target); },
      // drag(from, to, {modifiers}), drag({from, to}), or a continuous drag({path}).
      drag(from, to, o = {}) {
        const pt = (v) => Array.isArray(v) ? v : isObj(v) ? [v.x, v.y] : v;
        let p;
        if (isObj(from) && to === undefined) {
          o = pick(from, MOD_OPTS);
          if (from.path !== undefined) {
            if (from.from !== undefined || from.to !== undefined) throw new CuaError('invalid_params', 'drag: provide path or from/to, not both');
            p = { path: Array.isArray(from.path) ? from.path.map(pt) : from.path };
          } else {
            p = { from: pt(from.from), to: pt(from.to) };
          }
        } else {
          o = options(o, MOD_OPTS, 'drag');
          p = { from: pt(from), to: pt(to) };
        }
        const mods = modsOf(o, 'drag');
        if (mods) p.modifiers = mods;
        return act('drag', p);
      },
      // scroll(target, direction, distance = 1 page-ish unit, {modifiers}) or
      // scroll({x, y | element, direction, distance | pixels | scrollX/scrollY, modifiers})
      scroll(target, direction, distance = 1, o = {}) {
        let p, pixels;
        if (isObj(target) && direction === undefined) {
          const t = target;
          o = pick(t, MOD_OPTS);
          p = at(elementOf(t) !== undefined ? { element: elementOf(t) } : t, 'scroll');
          direction = t.direction;
          const sx = t.scrollX ?? t.scroll_x ?? 0, sy = t.scrollY ?? t.scroll_y ?? 0;
          if (direction === undefined && (sx || sy)) {
            direction = Math.abs(sy) >= Math.abs(sx) ? (sy > 0 ? 'down' : 'up') : (sx > 0 ? 'right' : 'left');
            pixels = Math.max(Math.abs(sx), Math.abs(sy));
          }
          if (t.pixels !== undefined) pixels = t.pixels;
          else if (pixels === undefined) distance = t.distance ?? t.amount ?? 1;
        } else {
          p = at(target, 'scroll');
          if (isObj(distance)) { o = pick(distance, MOD_OPTS); pixels = distance.pixels; }
          else o = options(o, MOD_OPTS, 'scroll');
        }
        if (typeof direction === 'string') direction = direction.toLowerCase();
        if (!SCROLL_DIRS.includes(direction)) throw new CuaError('invalid_params', 'scroll: direction must be up, down, left or right');
        if (pixels === undefined) pixels = Math.round(Number(distance) * 500);
        const mods = modsOf(o, 'scroll');
        if (mods) p.modifiers = mods;
        return act('scroll', Object.assign(p, { direction, pixels: Math.max(1, Math.round(pixels)) }));
      },
      typeText(text) {
        if (isObj(text)) text = text.text;
        if (typeof text !== 'string') throw new CuaError('invalid_params', 'typeText(text) expects a string');
        return act('type_text', { text });
      },
      // pressKey("ctrl+s"), pressKey(["ctrl", "s"]), pressKey({key}) or pressKey({keys}); {durationMs} holds the key.
      pressKey(key, o = {}) {
        if (isObj(key)) { if (Object.keys(o).length === 0) o = pick(key, HOLD_OPTS); key = key.key ?? key.keys; }
        o = options(o, HOLD_OPTS, 'pressKey');
        const p = { key: keyName(key) };
        const hold = holdOf(o);
        if (hold !== undefined) p.hold_ms = hold;
        return act('press_key', p);
      },
      paste(text, o = {}) {
        if (isObj(text)) { o = pick(text, ['format']); text = text.text; }
        o = options(o, ['format'], 'paste');
        if (o.format && o.format !== 'text') throw new CuaError('unsupported', 'Only plain-text paste is supported');
        return act('paste', { text });
      },
      // setValue(element | locator, value), setValue({element, value}) or setValue({role, name, value})
      setValue(element, value) {
        if (isObj(element) && value === undefined) {
          value = element.value;
          if (elementOf(element) !== undefined) element = elementOf(element);
          else { const { value: _, ...loc } = element; element = loc; }
        }
        return act('set_value', Object.assign(elementRef(element), { value: typeof value === 'number' || typeof value === 'boolean' ? String(value) : value }));
      },
      // performSecondaryAction(element | locator, action), ({element, action}) or ({role, name, action})
      performSecondaryAction(element, action) {
        if (isObj(element) && action === undefined) {
          if (elementOf(element) !== undefined) { action = element.action ?? element.name; element = elementOf(element); }
          else { const { action: a, ...loc } = element; action = a; element = loc; }
        }
        return act('perform_secondary_action', Object.assign(elementRef(element), { action }));
      },
    });
    return app;
  };

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
    return makeApp(await call('list_windows', q, 'getApp', emitOpt(o)));
  };
  // launchApp("drawing"): starts an installed application (desktop entry ID
  // such as org.gnome.Calculator, its last part, the menu name or the
  // program) on this desktop and returns its App once its window is up.
  cua.launchApp = async (name, o) => {
    o = options(o, ['emit'], 'launchApp');
    if (isObj(name)) name = name.app ?? name.appId ?? name.name ?? name.id;
    if (typeof name !== 'string' || !name.trim()) throw new CuaError('invalid_params', 'launchApp(name) expects the application\\'s name or desktop entry ID');
    return makeApp(await call('launch_app', { app: name }, 'launch', emitOpt(o)));
  };
  // The desktop's clipboard (plain text).
  cua.clipboard = Object.freeze({
    write(text) {
      if (isObj(text)) text = text.text;
      if (typeof text !== 'string') throw new CuaError('invalid_params', 'clipboard.write(text) expects a string');
      return call('clipboard_write', { text }, 'clipboard').then(() => undefined);
    },
    read() { return call('clipboard_read', {}, 'clipboard'); },
    release() { return call('clipboard_release', {}, 'clipboard').then(() => undefined); },
  });
  // Prints the API description again (after a context summary, say).
  cua.help = () => { write(helpText || 'no API description is available'); };
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

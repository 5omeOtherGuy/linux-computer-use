// Trusted browser host: drives one page tab of the operator's browser over
// CDP for model code (cua.getBrowser()). Every request is validated here;
// model code never gets page JavaScript execution: the only scripts run in
// the page are the fixed functions below, in an isolated world, with model
// strings passed as arguments. Page-derived text reaches the model only
// inside fenceUntrusted blocks.
import { CuaError } from '../errors.js';
import { fenceUntrusted } from '../format.js';
import { queryTree } from '../sandbox/runner.js';
import { CdpSession, listTabs, parseEndpoint } from './cdp.js';
import { capLines, renderAxTree, rectsFromSnapshot } from './ax.js';

const MAX_TEXT = 100_000;
const MAX_ID = 2 ** 31 - 1;
const WORLD = 'linux-cua';
const SETTLE_MS = 60;
const NAV_WAIT_MS = 10_000;
const READ_CHARS = 16_000;
const READ_LINKS = 40;

const sleep = (ms, signal) => new Promise((resolve, reject) => {
  if (signal?.aborted) { reject(new CuaError('cancelled', 'cancelled')); return; }
  const t = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve(); }, ms);
  const onAbort = () => { clearTimeout(t); reject(new CuaError('cancelled', 'cancelled')); };
  signal?.addEventListener('abort', onAbort, { once: true });
});

function fail(method, message) {
  throw new CuaError('invalid_params', `${method}: ${message}`);
}

function strict(method, params, allowed) {
  if (params === null || typeof params !== 'object' || Array.isArray(params)) fail(method, 'params must be an object');
  for (const k of Object.keys(params)) if (!allowed.includes(k)) fail(method, `unknown parameter ${String(k).slice(0, 40)}`);
  return params;
}

function elementId(method, v) {
  if (!Number.isInteger(v) || v < 1 || v > MAX_ID) fail(method, 'element must be an element ID from the page state you received');
  return v;
}

function coord(method, v) {
  if (!Number.isFinite(v) || v < 0 || v > 100_000) fail(method, 'x and y must be viewport pixels');
  return v;
}

function text(method, v, { allowEmpty = true } = {}) {
  if (typeof v !== 'string') fail(method, 'text must be a string');
  if (!allowEmpty && !v) fail(method, 'text must not be empty');
  if (v.length > MAX_TEXT) fail(method, `text exceeds ${MAX_TEXT} characters`);
  return v;
}

// Navigation targets: absolute http(s) URLs; file URLs only when the operator
// allows them (CUA_BROWSER_ALLOW_FILE=1), since a page can show any file the
// browser user can read.
export function checkUrl(raw, { allowFile = process.env.CUA_BROWSER_ALLOW_FILE === '1' } = {}) {
  if (typeof raw !== 'string' || !raw || raw.length > 8192 || /[\s\x00-\x1f\x7f]/.test(raw)) fail('goto', 'url must be an absolute http(s) or file URL without spaces');
  let u;
  try {
    u = new URL(raw);
  } catch {
    fail('goto', 'url must be an absolute http(s) or file URL');
  }
  if (u.protocol === 'file:' && !allowFile) fail('goto', 'file URLs are not allowed (the operator can allow them with CUA_BROWSER_ALLOW_FILE=1)');
  if (!['http:', 'https:', 'file:'].includes(u.protocol) || (u.protocol !== 'file:' && !u.hostname)) fail('goto', 'only http and https URLs can be opened');
  return u.href;
}

// Key names: "Return", "ctrl+a", "shift+Tab", "a", "F5", "PageDown".
const KEYS = {
  enter: ['Enter', 'Enter', 13, '\r'], return: ['Enter', 'Enter', 13, '\r'], kp_enter: ['Enter', 'NumpadEnter', 13, '\r'],
  tab: ['Tab', 'Tab', 9], escape: ['Escape', 'Escape', 27], esc: ['Escape', 'Escape', 27],
  backspace: ['Backspace', 'Backspace', 8], delete: ['Delete', 'Delete', 46], del: ['Delete', 'Delete', 46], insert: ['Insert', 'Insert', 45],
  home: ['Home', 'Home', 36], end: ['End', 'End', 35], pageup: ['PageUp', 'PageUp', 33], page_up: ['PageUp', 'PageUp', 33], prior: ['PageUp', 'PageUp', 33],
  pagedown: ['PageDown', 'PageDown', 34], page_down: ['PageDown', 'PageDown', 34], next: ['PageDown', 'PageDown', 34],
  left: ['ArrowLeft', 'ArrowLeft', 37], up: ['ArrowUp', 'ArrowUp', 38], right: ['ArrowRight', 'ArrowRight', 39], down: ['ArrowDown', 'ArrowDown', 40],
  arrowleft: ['ArrowLeft', 'ArrowLeft', 37], arrowup: ['ArrowUp', 'ArrowUp', 38], arrowright: ['ArrowRight', 'ArrowRight', 39], arrowdown: ['ArrowDown', 'ArrowDown', 40],
  space: [' ', 'Space', 32, ' '],
};
const PUNCT = { '-': ['Minus', 189], '=': ['Equal', 187], ',': ['Comma', 188], '.': ['Period', 190], '/': ['Slash', 191], ';': ['Semicolon', 186], "'": ['Quote', 222], '[': ['BracketLeft', 219], ']': ['BracketRight', 221], '\\': ['Backslash', 220], '`': ['Backquote', 192] };
const MOD_BITS = { alt: 1, ctrl: 2, control: 2, meta: 4, super: 4, cmd: 4, shift: 8 };
// Editing shortcuts Chromium only performs when told the editor command.
const COMMANDS = { 2: { a: 'selectAll', z: 'undo', y: 'redo', x: 'cut', c: 'copy', v: 'paste' }, 10: { z: 'redo' } };

export function parseKey(spec) {
  if (typeof spec !== 'string' || !spec || spec.length > 64) fail('pressKey', 'key must be a string such as "Return" or "ctrl+a"');
  const parts = spec === '+' ? ['+'] : spec.split('+');
  if (parts.at(-1) === '' && parts.length > 1) { parts.pop(); parts[parts.length - 1] = '+'; }
  const plain = parts.pop();
  let modifiers = 0;
  for (const m of parts) {
    const bit = MOD_BITS[m.toLowerCase()];
    if (!bit) fail('pressKey', `unknown modifier ${m.slice(0, 20)} (use ctrl, shift, alt, meta)`);
    modifiers |= bit;
  }
  let key, code, vk, txt;
  const named = KEYS[plain.toLowerCase()];
  if (named) [key, code, vk, txt] = named;
  else if (/^f([1-9]|1[0-2])$/i.test(plain)) { key = plain.toUpperCase(); code = key; vk = 111 + Number(plain.slice(1)); }
  else if ([...plain].length === 1) {
    const ch = plain;
    if (/[a-z]/i.test(ch)) { key = modifiers & 8 ? ch.toUpperCase() : ch.toLowerCase(); code = `Key${ch.toUpperCase()}`; vk = ch.toUpperCase().charCodeAt(0); }
    else if (/[0-9]/.test(ch)) { key = ch; code = `Digit${ch}`; vk = ch.charCodeAt(0); }
    else if (PUNCT[ch]) { key = ch; [code, vk] = PUNCT[ch]; }
    else { key = ch; code = ''; vk = 0; }
    txt = key;
  } else fail('pressKey', `unknown key ${plain.slice(0, 30)}`);
  const ev = { key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers };
  if (txt !== undefined && !(modifiers & 7)) { ev.text = txt; ev.unmodifiedText = txt; }
  const command = COMMANDS[modifiers]?.[key.toLowerCase()];
  if (command) ev.commands = [command];
  return ev;
}

// Fixed page functions (isolated world; `this` is the element's node).
const FN_CLICK_POINT = `function () {
  const el = this.nodeType === 1 ? this : this.parentElement;
  if (!el || !el.isConnected) return { error: 'detached' };
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  let r;
  if (this.nodeType === 3) { const range = document.createRange(); range.selectNodeContents(this); r = range.getBoundingClientRect(); }
  else r = el.getBoundingClientRect();
  if (!r.width || !r.height) return { error: 'hidden' };
  const x = r.x + r.width / 2, y = r.y + r.height / 2;
  if (x < 0 || y < 0 || x > innerWidth || y > innerHeight) return { error: 'outside', x, y };
  const root = el.getRootNode();
  const hit = (root && root.elementFromPoint ? root.elementFromPoint(x, y) : null) || document.elementFromPoint(x, y);
  const ok = hit && (hit === el || el.contains(hit) || hit.contains(el) && hit.getRootNode() !== root || (hit.control && hit.control === el) || (el.labels && [...el.labels].includes(hit)));
  if (!ok) return { error: 'covered', by: hit ? hit.tagName.toLowerCase() : null };
  return { x, y };
}`;

// The editable control for setValue: the element itself or its single
// editable descendant (a combobox wrapping its input).
const FN_EDIT_TARGET = `function () {
  const el = this.nodeType === 1 ? this : this.parentElement;
  if (!el || !el.isConnected) return 'detached';
  const NON_TEXT = ['checkbox', 'radio', 'button', 'submit', 'reset', 'file', 'image', 'hidden', 'range', 'color'];
  const editable = (e) => e.tagName === 'SELECT' || e.tagName === 'TEXTAREA' || (e.tagName === 'INPUT' && !NON_TEXT.includes(e.type)) || e.isContentEditable;
  let t = el;
  if (!editable(t)) {
    const inner = [...el.querySelectorAll('input, textarea, select, [contenteditable=""], [contenteditable=true]')].filter(editable);
    if (inner.length !== 1) return 'not_editable';
    t = inner[0];
  }
  if (t.disabled) return 'disabled';
  if (t.readOnly) return 'readonly';
  return t;
}`;
const FN_FOCUS = `function () {
  this.focus();
  let a = document.activeElement;
  while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
  return a === this || this.contains(a);
}`;
const FN_SELECT = `function (want) {
  const norm = (s) => String(s).replace(/\\s+/g, ' ').trim();
  const opts = [...this.options];
  let i = opts.findIndex((o) => norm(o.label) === norm(want));
  if (i < 0) i = opts.findIndex((o) => o.value === want);
  if (i < 0) return { error: 'no_option', count: opts.length };
  this.selectedIndex = i;
  this.dispatchEvent(new Event('input', { bubbles: true }));
  this.dispatchEvent(new Event('change', { bubbles: true }));
  return { value: opts[this.selectedIndex].label };
}`;
const FN_READBACK = `function () { return this.isContentEditable ? this.innerText : this.value; }`;
const FN_READ = `function (row, maxChars, maxLinks) {
  let el = this.nodeType === 1 ? this : this.parentElement;
  if (!el) return { error: 'detached' };
  if (row) { el = el.closest('li, tr, [role=row], [role=listitem], article'); if (!el) return { error: 'no_row' }; }
  const text = String(el.innerText || el.textContent || '');
  const anchors = [...(el.matches('a[href]') ? [el] : []), ...el.querySelectorAll('a[href]')];
  return {
    tag: el.tagName.toLowerCase(),
    text: text.slice(0, maxChars),
    ...(text.length > maxChars ? { truncated: text.length } : {}),
    ...(el.href ? { href: String(el.href) } : {}),
    links: anchors.slice(0, maxLinks).map((a) => ({ text: String(a.innerText || a.textContent || '').replace(/\\s+/g, ' ').trim().slice(0, 200), url: String(a.href) })),
    ...(anchors.length > maxLinks ? { moreLinks: anchors.length - maxLinks } : {}),
  };
}`;

const METHODS = ['open', 'observe', 'elements', 'info', 'click', 'set_value', 'type_text', 'press_key', 'goto', 'read', 'screenshot'];

export class BrowserHost {
  // endpoint: CUA_BROWSER_CDP (http://127.0.0.1:PORT); undefined disables the browser.
  constructor({ endpoint, log = () => {} } = {}) {
    this.rawEndpoint = endpoint;
    this.log = log;
    this.session = null;
    this.pinned = null; // target id of the tab model code drives
    this.queue = Promise.resolve();
    this.resetPageState();
  }

  resetPageState() {
    this.mainFrame = null;
    this.world = null;
    this.loading = false;
    this.navSeq = 0;
    this.domSeq = 0;
  }

  get methods() {
    return METHODS;
  }

  async #connect(signal, { repin = false } = {}) {
    if (this.session && !this.session.closed) return this.session;
    const endpoint = parseEndpoint(this.rawEndpoint);
    const tabs = await listTabs(endpoint, signal);
    let tab = this.pinned ? tabs.find((t) => t.id === this.pinned) : null;
    let note = null;
    if (!tab) {
      if (this.pinned && !repin) throw new CuaError('browser_tab_closed', 'the browser tab you were using is gone; call cua.getBrowser() again to use the current tab');
      if (!tabs.length) throw new CuaError('browser_unavailable', 'the browser has no open page tab');
      if (this.pinned) note = 'the previous tab is gone';
      tab = tabs[0];
      if (tabs.length > 1) note = `${note ? `${note}; ` : ''}the browser has ${tabs.length} tabs; using the first listed`;
    }
    const s = await CdpSession.connect(endpoint, tab);
    this.resetPageState();
    s.on('Page.frameStartedLoading', (p) => { if (p.frameId === this.mainFrame) { this.loading = true; this.navSeq++; } });
    s.on('Page.frameStoppedLoading', (p) => { if (p.frameId === this.mainFrame) this.loading = false; });
    s.on('Page.frameRequestedNavigation', (p) => { if (p.frameId === this.mainFrame) this.navSeq++; });
    s.on('Page.domContentEventFired', () => { this.domSeq++; });
    s.on('Page.frameNavigated', (p) => { if (!p.frame?.parentId) this.world = null; });
    s.on('Runtime.executionContextsCleared', () => { this.world = null; });
    await s.send('Page.enable');
    const tree = await s.send('Page.getFrameTree');
    this.mainFrame = tree.frameTree.frame.id;
    this.session = s;
    this.pinned = tab.id;
    this.connectNote = note;
    return s;
  }

  async #lock(fn) {
    const prev = this.queue;
    let release;
    this.queue = new Promise((r) => (release = r));
    try {
      await prev;
      return await fn();
    } finally {
      release();
    }
  }

  // One model request. Returns {value, outputs (fenced text when emit),
  // acted (input sent), observed (the model saw the full state),
  // screenshot, tab: {url, title}}.
  async call(method, params = {}, { signal, deadline = Date.now() + 30_000, emit = false } = {}) {
    if (!METHODS.includes(method)) throw new CuaError('invalid_params', `unknown browser method ${String(method).slice(0, 40)}`);
    const ctx = { signal, deadline, emit, method };
    return this.#lock(async () => {
      try {
        await this.#connect(signal, { repin: method === 'open' });
        const r = await this[`m_${method}`](params, ctx);
        if (!r.tab && method !== 'screenshot') r.tab = await this.#tabInfo(ctx);
        return r;
      } catch (err) {
        if (this.session?.closed) this.session = null;
        throw err;
      }
    });
  }

  #send(ctx, method, params = {}) {
    const left = ctx.deadline - Date.now();
    if (left <= 0) throw new CuaError('timeout', `${ctx.method}: the cell's deadline passed`);
    return this.session.send(method, params, { timeoutMs: Math.min(15_000, left) });
  }

  async #tabInfo(ctx) {
    const { targetInfo } = await this.#send(ctx, 'Target.getTargetInfo');
    return { url: String(targetInfo?.url ?? ''), title: String(targetInfo?.title ?? '') };
  }

  async #worldId(ctx) {
    if (this.world) return this.world;
    const r = await this.#send(ctx, 'Page.createIsolatedWorld', { frameId: this.mainFrame, worldName: WORLD, grantUniveralAccess: false });
    this.world = r.executionContextId;
    return this.world;
  }

  // A remote object id for an element ID, in the isolated world.
  async #resolve(ctx, id) {
    for (let attempt = 0; ; attempt++) {
      try {
        const r = await this.#send(ctx, 'DOM.resolveNode', { backendNodeId: id, executionContextId: await this.#worldId(ctx), objectGroup: WORLD });
        return r.object.objectId;
      } catch (err) {
        const m = String(err.cdpMessage ?? '');
        if (/No node with given id|Node with given id does not belong/i.test(m)) {
          throw new CuaError('stale_element', `${ctx.method}: element ${id} is not on the page (removed, or the page navigated since that state); look again`);
        }
        if (attempt === 0 && /context/i.test(m)) { this.world = null; continue; }
        throw err;
      }
    }
  }

  async #fn(ctx, objectId, fn, args = [], { byValue = true } = {}) {
    const r = await this.#send(ctx, 'Runtime.callFunctionOn', { objectId, functionDeclaration: fn, arguments: args.map((value) => ({ value })), returnByValue: byValue, objectGroup: WORLD });
    if (r.exceptionDetails) throw new CuaError('browser_error', `${ctx.method}: the page operation failed`);
    return byValue ? r.result.value : r.result;
  }

  #release(ctx) {
    this.session.send('Runtime.releaseObjectGroup', { objectGroup: WORLD }, { timeoutMs: 2000 }).catch(() => {});
  }

  // After input: a short settle, and when a navigation started, its
  // DOMContentLoaded (bounded).
  async #settle(ctx, navBefore, domBefore) {
    await sleep(SETTLE_MS, ctx.signal);
    if (this.navSeq === navBefore && !this.loading) return;
    const until = Math.min(ctx.deadline - 500, Date.now() + NAV_WAIT_MS);
    while (Date.now() < until && this.loading && this.domSeq === domBefore) await sleep(50, ctx.signal);
  }

  async #input(ctx, fn) {
    const nav = this.navSeq;
    const dom = this.domSeq;
    await this.#send(ctx, 'Page.bringToFront');
    await fn();
    await this.#settle(ctx, nav, dom);
  }

  async #key(ctx, spec) {
    const ev = parseKey(spec);
    await this.#send(ctx, 'Input.dispatchKeyEvent', { type: ev.text !== undefined ? 'keyDown' : 'rawKeyDown', ...ev });
    const { text: _t, unmodifiedText: _u, commands: _c, ...up } = ev;
    await this.#send(ctx, 'Input.dispatchKeyEvent', { type: 'keyUp', ...up });
  }

  async #snapshot(ctx) {
    const t0 = performance.now();
    const [ax, snap, metrics] = await Promise.all([
      this.#send(ctx, 'Accessibility.getFullAXTree'),
      this.#send(ctx, 'DOMSnapshot.captureSnapshot', { computedStyles: [] }),
      this.#send(ctx, 'Page.getLayoutMetrics'),
    ]);
    const vp = metrics.cssLayoutViewport || metrics.layoutViewport || {};
    const viewport = { width: Math.round(vp.clientWidth || 0), height: Math.round(vp.clientHeight || 0) };
    const tree = renderAxTree(ax.nodes, { rects: rectsFromSnapshot(snap), viewport });
    return { ...tree, viewport, ms: Math.round(performance.now() - t0) };
  }

  // The fenced page state for the model.
  async stateText(ctx, { query } = {}) {
    const snap = await this.#snapshot(ctx);
    const tab = await this.#tabInfo(ctx);
    const head = `page ${JSON.stringify(tab.title.slice(0, 200))} ${tab.url.slice(0, 500)} viewport ${snap.viewport.width}x${snap.viewport.height}`;
    let body = query ? queryTree(snap.lines.join('\n'), query) : capLines(snap.lines);
    if (query && body.length > 16_000) body = `${body.slice(0, 16_000)}\n# … truncated; use a more specific query`;
    const notes = [];
    if (this.connectNote) { notes.push(this.connectNote); this.connectNote = null; }
    const header = `browser tab${notes.length ? ` (${notes.join('; ')})` : ''}`;
    return { text: `${head}\n${body}`, output: `${header}\n${fenceUntrusted('browser', `${head}\n${body}`)}`, tab };
  }

  async m_open(params, ctx) {
    strict('getBrowser', params, []);
    if (!ctx.emit) return { value: null };
    const s = await this.stateText(ctx);
    return { value: null, outputs: [s.output], observed: true, tab: s.tab };
  }

  async m_observe(params, ctx) {
    strict('getAXState', params, ['query']);
    if (params.query !== undefined && (typeof params.query !== 'string' || params.query.length > 500)) fail('getAXState', 'query must be a short string');
    const query = params.query?.trim() || undefined;
    const s = await this.stateText(ctx, { query });
    return { value: s.text, outputs: ctx.emit ? [s.output] : [], observed: ctx.emit && !query, tab: s.tab };
  }

  // Elements for locators, find() and waitFor(); not shown to the model.
  async m_elements(params, ctx) {
    strict('find', params, []);
    const snap = await this.#snapshot(ctx);
    return { value: { elements: snap.elements } };
  }

  async m_info(params, ctx) {
    strict('info', params, []);
    let ready = 'unknown';
    try {
      const r = await this.#send(ctx, 'Runtime.evaluate', { expression: 'document.readyState', contextId: await this.#worldId(ctx), returnByValue: true });
      if (typeof r.result?.value === 'string') ready = r.result.value;
    } catch {
      this.world = null; // between documents
      ready = 'loading';
    }
    const tab = await this.#tabInfo(ctx);
    return { value: { ...tab, ready }, tab };
  }

  async m_click(params, ctx) {
    strict('click', params, ['element_index', 'x', 'y', 'button', 'click_count', 'modifiers']);
    const button = params.button ?? 'left';
    if (!['left', 'right', 'middle'].includes(button)) fail('click', 'button must be left, right or middle');
    const count = params.click_count ?? 1;
    if (!Number.isInteger(count) || count < 1 || count > 3) fail('click', 'clickCount must be 1, 2 or 3');
    let modifiers = 0;
    for (const m of params.modifiers ?? []) {
      const bit = MOD_BITS[String(m).toLowerCase()];
      if (!bit) fail('click', 'modifiers must be shift, ctrl, alt or meta');
      modifiers |= bit;
    }
    let x, y;
    if (params.element_index !== undefined) {
      const id = elementId('click', params.element_index);
      const obj = await this.#resolve(ctx, id);
      const p = await this.#fn(ctx, obj, FN_CLICK_POINT);
      this.#release(ctx);
      if (p?.error === 'hidden') throw new CuaError('element_hidden', `click: element ${id} has no visible box (hidden, or a styled control: click its visible label instead)`);
      if (p?.error === 'covered') throw new CuaError('element_covered', `click: element ${id} is covered by another element${/^[a-z0-9-]{1,30}$/.test(p.by ?? '') ? ` (<${p.by}>)` : ''} at its centre; close the overlay, or click what covers it`);
      if (p?.error === 'outside') throw new CuaError('element_hidden', `click: element ${id} could not be scrolled into view`);
      if (p?.error) throw new CuaError('stale_element', `click: element ${id} is no longer in the page`);
      ({ x, y } = p);
    } else {
      if (params.x === undefined || params.y === undefined) fail('click', 'target must be an element ID, a locator or [x, y]');
      x = coord('click', params.x);
      y = coord('click', params.y);
    }
    await this.#input(ctx, async () => {
      await this.#send(ctx, 'Input.dispatchMouseEvent', { type: 'mouseMoved', x, y, modifiers });
      for (let i = 1; i <= count; i++) {
        await this.#send(ctx, 'Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button, clickCount: i, modifiers });
        await this.#send(ctx, 'Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button, clickCount: i, modifiers });
      }
    });
    return { value: undefined, acted: true };
  }

  // agent-desktop's fill: focus, select all, insert, exact readback.
  async m_set_value(params, ctx) {
    strict('setValue', params, ['element_index', 'value']);
    const id = elementId('setValue', params.element_index);
    const value = text('setValue', params.value);
    const obj = await this.#resolve(ctx, id);
    const target = await this.#fn(ctx, obj, FN_EDIT_TARGET, [], { byValue: false });
    if (target.type === 'string') {
      this.#release(ctx);
      const why = { detached: 'is no longer in the page', not_editable: 'is not an editable field (and contains no single one)', disabled: 'is disabled', readonly: 'is read-only' }[target.value] || 'cannot be edited';
      throw new CuaError(target.value === 'detached' ? 'stale_element' : 'not_editable', `setValue: element ${id} ${why}`);
    }
    const tid = target.objectId;
    let result;
    await this.#input(ctx, async () => {
      if (target.description === 'select' || /^select/.test(String(target.description))) {
        const r = await this.#fn(ctx, tid, FN_SELECT, [value]);
        if (r?.error === 'no_option') throw new CuaError('no_such_option', `setValue: the list has no option labelled or valued ${JSON.stringify(value.slice(0, 80))} (${r.count} options; read the state for their names)`);
        result = r.value;
        return;
      }
      if (!(await this.#fn(ctx, tid, FN_FOCUS))) throw new CuaError('focus_failed', `setValue: element ${id} did not take the keyboard focus; no text was inserted`);
      await this.#key(ctx, 'ctrl+a');
      if (value) await this.#send(ctx, 'Input.insertText', { text: value });
      else await this.#key(ctx, 'Delete');
      result = await this.#fn(ctx, tid, FN_READBACK);
    });
    this.#release(ctx);
    const norm = (s) => String(s ?? '').replace(/\r\n/g, '\n').replace(/\n$/, '');
    if (typeof result !== 'string' || (norm(result) !== norm(value) && !(target.description?.startsWith('select') && result))) {
      throw new CuaError('value_mismatch', `setValue: element ${id} reads back ${typeof result === 'string' ? `${result.length} characters` : 'nothing'} instead of the ${value.length} requested (the page reformatted or rejected the input); look at the state before continuing`, { readback: typeof result === 'string' ? result.slice(0, 500) : null });
    }
    return { value: undefined, acted: true };
  }

  async m_type_text(params, ctx) {
    strict('typeText', params, ['text']);
    const t = text('typeText', params.text, { allowEmpty: false });
    await this.#input(ctx, () => this.#send(ctx, 'Input.insertText', { text: t }));
    return { value: undefined, acted: true };
  }

  async m_press_key(params, ctx) {
    strict('pressKey', params, ['key']);
    parseKey(params.key);
    await this.#input(ctx, () => this.#key(ctx, params.key));
    return { value: undefined, acted: true };
  }

  async m_goto(params, ctx) {
    strict('goto', params, ['url', 'timeout_ms']);
    const url = checkUrl(params.url);
    const timeout = params.timeout_ms ?? 15_000;
    if (!Number.isInteger(timeout) || timeout < 100 || timeout > 120_000) fail('goto', 'timeoutMs must be an integer in [100, 120000]');
    await this.#send(ctx, 'Page.bringToFront');
    const r = await this.#send(ctx, 'Page.navigate', { url });
    if (r.errorText) throw new CuaError('navigation_failed', `goto: navigation failed (${String(r.errorText).slice(0, 100)})`);
    if (r.isDownload) throw new CuaError('navigation_failed', 'goto: the URL started a download instead of a page');
    this.world = null;
    const until = Math.min(ctx.deadline - 200, Date.now() + timeout);
    for (;;) {
      const { frameTree } = await this.#send(ctx, 'Page.getFrameTree');
      if (!r.loaderId || frameTree.frame.loaderId === r.loaderId) {
        const info = (await this.m_info({}, ctx)).value;
        if (info.ready !== 'loading') return { value: { url: info.url, title: info.title }, acted: true };
      }
      if (Date.now() >= until) throw new CuaError('timeout', `goto: the page did not finish loading within ${timeout} ms`);
      await sleep(50, ctx.signal);
    }
  }

  async m_read(params, ctx) {
    strict('read', params, ['element_index', 'row']);
    const id = elementId('read', params.element_index);
    const obj = await this.#resolve(ctx, id);
    const r = await this.#fn(ctx, obj, FN_READ, [params.row === true, READ_CHARS, READ_LINKS]);
    this.#release(ctx);
    if (r?.error === 'no_row') throw new CuaError('element_not_found', `read: element ${id} is not inside a list item, table row or article; read it or an ancestor without {row: true}`);
    if (!r || r.error) throw new CuaError('stale_element', `read: element ${id} is no longer in the page`);
    return { value: r, outputs: ctx.emit ? [fenceUntrusted('page_text', JSON.stringify(r))] : [] };
  }

  async m_screenshot(params, ctx) {
    strict('getScreenshot', params, []);
    await this.#send(ctx, 'Page.bringToFront');
    const r = await this.#send(ctx, 'Page.captureScreenshot', { format: 'png' });
    const head = Buffer.from(String(r.data).slice(0, 32), 'base64');
    return { value: undefined, screenshot: { data: r.data, mimeType: 'image/png', width: head.readUInt32BE(16), height: head.readUInt32BE(20) } };
  }

  close() {
    this.session?.close();
    this.session = null;
  }
}

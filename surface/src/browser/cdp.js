// Chrome DevTools Protocol connection to one page tab of a browser whose
// debugging endpoint the operator configured (CUA_BROWSER_CDP). Only loopback
// endpoints are accepted, and the page's WebSocket must be on that same
// host and port. Uses Node's built-in fetch and WebSocket.
import { CuaError } from '../errors.js';

const LOOPBACK = ['127.0.0.1', 'localhost', '[::1]'];
const DEFAULT_TIMEOUT_MS = 15_000;

// "http://127.0.0.1:9222" -> {base, host}; anything else is refused.
export function parseEndpoint(raw) {
  if (raw === undefined || raw === null || raw === '') {
    throw new CuaError('browser_unavailable', 'no browser is configured: start a Chromium-family browser with --remote-debugging-port and set CUA_BROWSER_CDP=http://127.0.0.1:PORT for the server');
  }
  let u;
  try {
    u = new URL(String(raw));
  } catch {
    throw new CuaError('browser_unavailable', 'CUA_BROWSER_CDP is not a URL (expected http://127.0.0.1:PORT)');
  }
  if (u.protocol !== 'http:' || !LOOPBACK.includes(u.hostname) || !u.port || u.username || u.password || (u.pathname !== '/' && u.pathname !== '') || u.search || u.hash) {
    throw new CuaError('browser_unavailable', 'CUA_BROWSER_CDP must be a loopback endpoint http://127.0.0.1:PORT');
  }
  const host = `${u.hostname}:${u.port}`;
  return { base: `http://${host}`, host };
}

async function getJson(url, signal) {
  let res;
  try {
    res = await fetch(url, { signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(5000)]) : AbortSignal.timeout(5000) });
  } catch (err) {
    throw new CuaError('browser_unavailable', `the browser's debugging endpoint does not answer (${err.message})`);
  }
  if (!res.ok) throw new CuaError('browser_unavailable', `the browser's debugging endpoint answered HTTP ${res.status}`);
  return res.json();
}

// Page tabs the endpoint lists (no DevTools or extension pages).
export async function listTabs(endpoint, signal) {
  const list = await getJson(`${endpoint.base}/json/list`, signal);
  if (!Array.isArray(list)) throw new CuaError('protocol_error', 'unexpected /json/list answer');
  return list.filter((t) => t && t.type === 'page' && typeof t.id === 'string' && typeof t.webSocketDebuggerUrl === 'string'
    && !/^(devtools|chrome-extension|chrome-untrusted):/.test(String(t.url)));
}

export class CdpSession {
  constructor(ws, targetId) {
    this.ws = ws;
    this.targetId = targetId;
    this.seq = 0;
    this.pending = new Map();
    this.listeners = new Map();
    this.closed = false;
    ws.addEventListener('message', (ev) => this.#onMessage(ev.data));
    ws.addEventListener('close', () => this.#onClose());
    ws.addEventListener('error', () => this.#onClose());
  }

  static async connect(endpoint, tab) {
    const prefix = `ws://${endpoint.host}/devtools/page/`;
    if (!tab.webSocketDebuggerUrl.startsWith(prefix)) throw new CuaError('browser_unavailable', 'the page\'s debugging socket is not on the configured loopback endpoint; refused');
    const ws = new WebSocket(tab.webSocketDebuggerUrl);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => { ws.close(); reject(new CuaError('browser_unavailable', 'the page\'s debugging socket did not open')); }, 5000);
      ws.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
      ws.addEventListener('error', () => { clearTimeout(timer); reject(new CuaError('browser_unavailable', 'the page\'s debugging socket failed to open')); }, { once: true });
    });
    return new CdpSession(ws, tab.id);
  }

  #onMessage(data) {
    let msg;
    try {
      msg = JSON.parse(typeof data === 'string' ? data : Buffer.from(data).toString('utf8'));
    } catch {
      return;
    }
    if (msg.id !== undefined) {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.error) p.reject(Object.assign(new CuaError('cdp_error', `${p.method}: ${String(msg.error.message).slice(0, 200)}`), { cdpMessage: String(msg.error.message) }));
      else p.resolve(msg.result ?? {});
      return;
    }
    for (const fn of this.listeners.get(msg.method) || []) fn(msg.params ?? {});
  }

  #onClose() {
    if (this.closed) return;
    this.closed = true;
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(new CuaError('browser_tab_closed', `${p.method}: the connection to the tab closed (tab closed or browser exited)`));
    }
    this.pending.clear();
    for (const fn of this.listeners.get('close') || []) fn();
  }

  on(event, fn) {
    if (!this.listeners.has(event)) this.listeners.set(event, []);
    this.listeners.get(event).push(fn);
  }

  send(method, params = {}, { timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
    if (this.closed) return Promise.reject(new CuaError('browser_tab_closed', `${method}: the connection to the tab is closed`));
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new CuaError('timeout', `${method}: the browser did not answer within ${Math.round(timeoutMs)} ms (a JavaScript dialog may block the page); inspect the state before retrying input`));
      }, Math.max(1, timeoutMs));
      this.pending.set(id, { resolve, reject, timer, method });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    try {
      this.ws.close();
    } catch {}
    this.#onClose();
  }
}

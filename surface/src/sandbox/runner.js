// Trusted parent side of the isolated JS REPL.
//
// The REPL runs in a separate Node process (sandbox/child.js) started with
// the Node permission model and an empty environment; inside that process
// model code runs in a QuickJS WebAssembly VM. Node's `vm` module is not used
// and is not a security boundary. The runner:
//   - forwards host calls of the *active* cell to HostApi (which revalidates
//     and authorizes them) and rejects calls from finished or stale cells;
//   - cancels in-flight host calls when a cell finishes, times out or is
//     cancelled (HostApi/EngineClient kill the daemon if a request is in flight);
//   - kills the sandbox process if it does not stop within a grace period,
//     reporting that REPL state was lost;
//   - presents results in the OpenAI-compatible shapes (WindowInfo, AppInfo,
//     AX text, PNG bytes) and auto-emits observations into the cell output,
//     fencing screen content as untrusted.
import { fork } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CuaError, toCuaError } from '../errors.js';
import { appInfos, fenceUntrusted, isScreenshot, windowInfo } from '../format.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const SURFACE_ROOT = path.resolve(here, '..', '..');
const CHILD = path.join(here, 'child.js');
// Screenshots the host keeps so model code can emit them without re-encoding;
// bounded by total size (base64 characters), oldest dropped first.
const MAX_HELD_IMAGE_CHARS = 64_000_000;
const MAX_CELL_IMAGES = 16;
const STDERR_TAIL = 4000;
const MAX_EMITTED_IMAGE_CHARS = 16_000_000;
const VIEWS = ['windows', 'apps', 'state', 'getApp', 'ax', 'screenshot', 'action'];
const ACTIONS = ['click', 'drag', 'scroll', 'type_text', 'press_key', 'paste', 'set_value', 'perform_secondary_action', 'mouse_move'];
const KILL_GRACE_MS = 1500;
const CANCEL_GRACE_MS = 750;

export class SandboxRunner {
  constructor({ host, memoryLimitMb = 128, log = () => {} }) {
    this.host = host;
    this.memoryLimitMb = memoryLimitMb;
    this.log = log;
    this.child = null;
    this.ready = null;
    this.cell = null;
    this.seq = 0;
    this.images = new Map();
    this.imageChars = 0;
    this.imageSeq = 0;
    this.stderrTail = '';
    this.fatal = null;
    this.stateLost = false;
    this.generation = 0;
  }

  #spawn() {
    const gen = ++this.generation;
    const child = fork(CHILD, [], {
      cwd: SURFACE_ROOT,
      env: {},
      execArgv: [
        '--permission',
        `--allow-fs-read=${SURFACE_ROOT}`,
        '--max-old-space-size=256',
        '--disallow-code-generation-from-strings',
      ],
      stdio: ['ignore', 'ignore', 'pipe', 'ipc'],
      serialization: 'json',
    });
    this.stderrTail = '';
    this.fatal = null;
    child.stderr.on('data', (d) => {
      const text = d.toString('utf8');
      this.stderrTail = (this.stderrTail + text).slice(-STDERR_TAIL);
      this.log(`sandbox: ${text.trimEnd()}`);
    });
    child.on('message', (msg) => this.#onMessage(gen, msg));
    child.on('exit', (code, signal) => {
      if (this.child !== child) return;
      this.child = null;
      this.ready = null;
      this.stateLost = true;
      this.log(`sandbox exited (${signal || code})`);
      // The cause, when the sandbox reported one, goes into the cell result.
      const cause = (this.fatal || this.stderrTail.trim()).slice(-1500);
      if (this.cell) this.#complete(this.cell, { ok: false, error: `SandboxError: sandbox process exited (${signal || code}); REPL state was lost${cause ? `\n${cause}` : ''}`, stateLost: true });
    });
    this.child = child;
    this.ready = new Promise((resolve, reject) => {
      this.readyWaiter = { resolve, reject };
      child.once('exit', () => reject(new CuaError('sandbox_failed', 'sandbox process exited during startup')));
    });
    child.send({ type: 'init', methods: this.host.methods, memoryLimitMb: this.memoryLimitMb });
    return this.ready;
  }

  #kill(reason) {
    const child = this.child;
    if (!child) return;
    this.log(`sandbox: killing (${reason})`);
    this.child = null;
    this.ready = null;
    try {
      child.kill('SIGKILL');
    } catch {}
  }

  #onMessage(gen, msg) {
    if (gen !== this.generation || !msg || typeof msg !== 'object') return;
    switch (msg.type) {
      case 'ready':
        this.readyWaiter?.resolve();
        break;
      case 'fatal':
        this.fatal = String(msg.error).slice(0, 4000);
        this.log(`sandbox fatal: ${msg.error}`);
        break;
      case 'host_call':
        this.#hostCall(msg);
        break;
      case 'emit':
        this.#emit(msg);
        break;
      case 'done':
        if (this.cell && this.cell.id === msg.id) this.#finish(this.cell, msg);
        break;
      default:
        break;
    }
  }

  #reply(callId, payload) {
    const child = this.child;
    if (child && child.connected) child.send({ type: 'host_result', callId, ...payload });
  }

  async #hostCall(msg) {
    const cell = this.cell;
    if (!cell || cell.finished || msg.evalId !== cell.id) {
      this.#reply(msg.callId, { ok: false, error: { code: 'no_active_cell', message: 'host call rejected: its cell is no longer running' } });
      return;
    }
    cell.inflight++;
    try {
      let params, opts;
      try {
        params = JSON.parse(typeof msg.params === 'string' ? msg.params : '{}');
        opts = JSON.parse(typeof msg.opts === 'string' ? msg.opts : '{}');
      } catch {
        throw new CuaError('invalid_params', 'params are not valid JSON');
      }
      const method = typeof msg.method === 'string' ? msg.method : '';
      const view = opts?.view;
      if (!VIEWS.includes(view)) throw new CuaError('invalid_params', 'unknown result view');
      const emit = opts.emit === true;
      const value = await this.#view(cell, method, params, view, emit, opts);
      if (cell.finished) throw new CuaError('cell_finished', 'the cell finished before this call completed');
      this.#reply(msg.callId, { ok: true, value });
    } catch (err) {
      const e = toCuaError(err);
      this.#reply(msg.callId, { ok: false, error: e.toJSON() });
    } finally {
      cell.inflight--;
    }
  }

  async #call(cell, method, params) {
    const result = await this.host.call(method, params, { signal: cell.controller.signal, deadline: cell.deadline });
    if (cell.finished) throw new CuaError('cell_finished', 'the cell finished before this call completed');
    cell.calls.push(method);
    return result;
  }

  #emitText(cell, text) {
    cell.outputs.push({ type: 'text', text });
  }

  // Runs one host call and shapes its result for the VM and the cell output.
  async #view(cell, method, params, view, emit, opts = {}) {
    switch (view) {
      case 'windows': case 'apps': case 'state': {
        // AppInfo is built from the window list (it carries the windows).
        if (method !== 'list_windows') throw new CuaError('invalid_params', 'method does not match view');
        const raw = await this.#call(cell, method, params);
        const windows = Array.isArray(raw) ? raw.map(windowInfo) : [];
        const value = view === 'windows' ? windows : view === 'apps' ? appInfos(windows) : { apps: appInfos(windows), browsers: [] };
        if (emit) this.#emitText(cell, fenceUntrusted(view, JSON.stringify(value)));
        return value;
      }
      case 'getApp': {
        if (method !== 'list_windows' || !params || typeof params !== 'object') throw new CuaError('invalid_params', 'getApp requires an app id, {appId} or {windowId}');
        const { windowId, appId } = params;
        if (windowId !== undefined && (!Number.isInteger(windowId) || windowId < 0 || windowId > 0xffffffff)) throw new CuaError('invalid_params', 'getApp: windowId must be a WindowInfo id from cua.listWindows()');
        if (appId !== undefined && typeof appId !== 'string') throw new CuaError('invalid_params', 'getApp: appId must be a string');
        const raw = await this.#call(cell, 'list_windows', {});
        const window = windowId !== undefined ? raw.find((w) => w.window_id === windowId) : mainWindow(raw, appId);
        if (!window) {
          const apps = [...new Set(raw.map((w) => String(w.app_id)))].join(', ');
          throw new CuaError('window_not_found', windowId !== undefined
            ? `no listed window has id ${windowId}; open windows belong to: ${apps.slice(0, 300)}`
            : `no app ${JSON.stringify(appId).slice(0, 80)} is open; open apps: ${apps.slice(0, 300)}`);
        }
        // The App covers all windows of that process (dialogs included).
        const windows = raw.filter((w) => w.app_id === window.app_id && w.pid === window.pid).map(windowInfo);
        const app = { ...appInfos(windows)[0], windows };
        if (emit) this.#emitText(cell, fenceUntrusted('app', JSON.stringify(app)));
        return { window: windowInfo(window), app };
      }
      case 'ax': {
        if (method !== 'get_ax_state') throw new CuaError('invalid_params', 'method does not match view');
        const obs = await this.#call(cell, method, params);
        const query = typeof opts.query === 'string' && opts.query.trim() ? opts.query.trim() : null;
        const text = query ? queryTree(obs.accessibility, query) : this.host.axView(obs, { diff: !opts.disableDiffing, commit: emit });
        if (emit) {
          this.#emitText(cell, observationHeader(obs));
          this.#emitText(cell, fenceUntrusted('accessibility', text));
          if (!query && cell.pending?.window?.window_id === obs.window?.window_id) cell.pending = null;
        }
        return text;
      }
      case 'screenshot': {
        if (method !== 'get_screenshot') throw new CuaError('invalid_params', 'method does not match view');
        const shot = await this.#call(cell, method, params);
        if (!isScreenshot(shot)) throw new CuaError('protocol_error', 'engine returned no screenshot');
        const id = this.#storeImage(shot);
        if (emit) cell.outputs.push({ type: 'image', id });
        // The sandbox decodes $bytes natively and hands model code a Uint8Array.
        return { $bytes: shot.data, $image: id };
      }
      case 'action': {
        if (!ACTIONS.includes(method)) throw new CuaError('invalid_params', 'not an action');
        const obs = await this.#call(cell, method, params);
        if (obs && typeof obs === 'object' && obs.window) {
          cell.pending = obs;
          cell.pendingTarget = params.app ? { window_id: params.window_id, app: true } : { window_id: obs.window.window_id };
        }
        if (Array.isArray(obs?.warnings) && obs.warnings.length) {
          this.#emitText(cell, `warnings after ${method}: ${fenceUntrusted('warnings', obs.warnings.map(String).join('\n'))}`);
        }
        return undefined;
      }
      default:
        throw new CuaError('invalid_params', 'unknown result view');
    }
  }

  #storeImage(shot) {
    const id = `img_${++this.imageSeq}`;
    this.images.set(id, { data: shot.data, mimeType: shot.mimeType, width: shot.width, height: shot.height });
    this.imageChars += shot.data.length;
    while (this.imageChars > MAX_HELD_IMAGE_CHARS && this.images.size > 1) {
      const [oldest, img] = this.images.entries().next().value;
      this.images.delete(oldest);
      this.imageChars -= img.data.length;
    }
    return id;
  }

  #emit(msg) {
    const cell = this.cell;
    if (!cell || cell.finished || msg.evalId !== cell.id) return;
    let payload;
    try {
      payload = JSON.parse(msg.payload);
    } catch {
      return;
    }
    if (msg.kind === 'text') {
      const level = String(payload.level || 'log');
      const text = String(payload.text ?? '');
      cell.outputs.push({ type: 'text', text: level === 'log' ? text : `[${level}] ${text}` });
    } else if (msg.kind === 'image') {
      if (typeof payload.id === 'string') {
        if (this.images.has(payload.id)) cell.outputs.push({ type: 'image', id: payload.id });
        else cell.outputs.push({ type: 'text', text: '[warn] emitImage(): that screenshot is no longer held by the host' });
      } else if (payload.tooLarge) {
        cell.outputs.push({ type: 'text', text: '[warn] emitImage(): image too large' });
      } else if (typeof payload.data === 'string' && ['image/png', 'image/jpeg'].includes(payload.mimeType)
        && payload.data.length <= MAX_EMITTED_IMAGE_CHARS && /^[A-Za-z0-9+/]*={0,2}$/.test(payload.data)) {
        cell.outputs.push({ type: 'image', id: this.#storeImage({ data: payload.data, mimeType: payload.mimeType }) });
      } else {
        cell.outputs.push({ type: 'text', text: '[warn] emitImage(): unsupported image' });
      }
    }
  }

  // The state shown after the cell is observed afresh when the cell ends:
  // apps often finish reacting (a dialog closing after Save) after the last
  // action's settle.
  async #finish(cell, msg) {
    if (cell.pending && cell.pendingTarget && !isScreenshot(cell.pending.screenshot) && !msg.reason && Date.now() < cell.deadline) {
      try {
        const obs = await this.host.call('get_ax_state', cell.pendingTarget, { signal: cell.controller.signal, deadline: Math.min(cell.deadline, Date.now() + 3000) });
        if (obs?.window) cell.pending = obs;
      } catch {
        // keep the action's own observation
      }
    }
    this.#complete(cell, msg);
  }

  #complete(cell, msg) {
    if (cell.finished) return;
    cell.finished = true;
    clearTimeout(cell.hardTimer);
    clearTimeout(cell.cancelTimer);
    // Anything this cell started but did not await is cancelled now.
    if (cell.inflight > 0) cell.cancelledCalls += cell.inflight;
    cell.controller.abort();
    if (this.cell === cell) this.cell = null;
    // The state after the cell's last action, unless the cell emitted a
    // newer observation of that window itself.
    let observation;
    if (cell.pending) {
      const obs = cell.pending;
      observation = { header: observationHeader(obs), accessibility: this.host.axView(obs, { diff: true, shortest: true, commit: true }) };
      if (isScreenshot(obs.screenshot)) cell.outputs.push({ type: 'image', id: this.#storeImage(obs.screenshot) });
    }
    let images = 0;
    const outputs = [];
    for (const o of cell.outputs) {
      if (o.type !== 'image') { outputs.push(o); continue; }
      const img = this.images.get(o.id);
      if (!img) continue;
      if (++images > MAX_CELL_IMAGES) { outputs.push({ type: 'text', text: `[warn] more than ${MAX_CELL_IMAGES} images; the rest were omitted` }); break; }
      outputs.push({ type: 'image', data: img.data, mimeType: img.mimeType });
    }
    cell.resolve({
      ok: !!msg.ok,
      value: msg.value,
      error: msg.error,
      reason: msg.reason,
      stateLost: !!msg.stateLost,
      outputs,
      calls: cell.calls,
      cancelledCalls: cell.cancelledCalls,
      observation,
    });
  }

  async eval({ code, timeoutMs, signal }) {
    if (typeof code !== 'string') throw new CuaError('invalid_params', 'code must be a string');
    if (this.cell) return { ok: false, error: 'BusyError: another cell is running', outputs: [], calls: [], cancelledCalls: 0 };
    const lostBefore = this.stateLost;
    this.stateLost = false;
    if (!this.child) await this.#spawn();
    else await this.ready;
    const child = this.child;
    if (!child) throw new CuaError('sandbox_failed', 'sandbox process is not running');
    const id = ++this.seq;
    const result = await new Promise((resolve) => {
      const cell = {
        id,
        resolve,
        deadline: Date.now() + timeoutMs,
        controller: new AbortController(),
        outputs: [],
        calls: [],
        inflight: 0,
        cancelledCalls: 0,
        finished: false,
        pending: null,
      };
      this.cell = cell;
      // Backstop: a sandbox that does not report back in time is killed.
      cell.hardTimer = setTimeout(() => {
        this.#kill('cell did not stop after its timeout');
        this.#complete(cell, { ok: false, error: 'TimeoutError: cell exceeded timeout_ms; sandbox killed', reason: 'timeout', stateLost: true });
        this.stateLost = false;
      }, timeoutMs + KILL_GRACE_MS);
      const onAbort = () => {
        if (cell.finished) return;
        cell.controller.abort(); // stop host work immediately (kills the daemon if a request is in flight)
        if (child.connected) child.send({ type: 'cancel', id });
        cell.cancelTimer = setTimeout(() => {
          this.#kill('cell did not stop after cancellation');
          this.#complete(cell, { ok: false, error: 'CancelledError: cell cancelled; sandbox killed', reason: 'cancelled', stateLost: true });
          this.stateLost = false;
        }, CANCEL_GRACE_MS);
      };
      child.send({ type: 'eval', id, code, timeoutMs });
      if (signal) {
        if (signal.aborted) onAbort();
        else signal.addEventListener('abort', onAbort, { once: true });
      }
    });
    if (result.stateLost) this.stateLost = false;
    result.stateLostBefore = lostBefore;
    return result;
  }

  cancelActive() {
    if (this.cell) this.reset();
  }

  reset() {
    this.#kill('js_reset');
    this.images.clear();
    this.imageChars = 0;
    this.stateLost = false;
    if (this.cell) this.#complete(this.cell, { ok: false, error: 'ResetError: REPL was reset', reason: 'reset', stateLost: true });
  }

  async close() {
    this.#kill('shutdown');
  }
}

function observationHeader(obs) {
  const w = obs.window || {};
  const lines = [`window ${w.window_id} (${w.width}x${w.height})${obs.closed ? ' closed' : ''}`];
  const others = Array.isArray(obs.app_windows) ? obs.app_windows : [];
  if (others.length) {
    const list = others.map((o) => `${o.window_id} ${JSON.stringify(String(o.title ?? '')).slice(0, 80)}`).join('; ');
    lines.push(`other windows of this app: ${fenceUntrusted('windows', list)}`);
  }
  if (obs.timing) lines.push(`timing: settle_ms=${round(obs.timing.settle_ms)} observation_ms=${round(obs.timing.observation_ms)}`);
  if (Array.isArray(obs.warnings) && obs.warnings.length) lines.push(`warnings: ${fenceUntrusted('warnings', obs.warnings.map(String).join('\n'))}`);
  return lines.join('\n');
}

const round = (v) => (typeof v === 'number' ? Math.round(v) : v);

// The app's main window for getApp(appId): matched case-insensitively on the
// app id; a window that is not a dialog (no WM_TRANSIENT_FOR) wins.
function mainWindow(raw, appId) {
  const want = String(appId).toLowerCase();
  const mine = raw.filter((w) => String(w.app_id).toLowerCase() === want);
  return mine.find((w) => !w.transient_for) || mine[0];
}

// Subtrees of an accessibility tree whose lines match `query` (case-
// insensitive substring): each match with its ancestors and descendants.
export function queryTree(text, query, maxLines = 400) {
  const lines = String(text ?? '').split('\n');
  const depth = (l) => l.length - l.trimStart().length;
  const q = query.toLowerCase();
  const keep = new Set();
  for (let i = 0; i < lines.length; i++) {
    if (!/^\s*\[\d+\]/.test(lines[i]) || !lines[i].toLowerCase().includes(q)) continue;
    const d = depth(lines[i]);
    keep.add(i);
    for (let j = i - 1, need = d; j >= 0 && need > 0; j--) if (depth(lines[j]) < need) { keep.add(j); need = depth(lines[j]); }
    for (let j = i + 1; j < lines.length && depth(lines[j]) > d; j++) keep.add(j);
  }
  const out = [...keep].sort((a, b) => a - b).map((i) => lines[i]);
  if (out.length === 0) return `# no element matches ${JSON.stringify(query)}`;
  const head = `# elements matching ${JSON.stringify(query)} with their ancestors and descendants`;
  return [head, ...out.slice(0, maxLines), ...(out.length > maxLines ? [`… ${out.length - maxLines} more lines`] : [])].join('\n');
}

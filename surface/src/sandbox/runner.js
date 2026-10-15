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
//   - keeps screenshots host-side and gives the VM opaque image handles.
import { fork } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CuaError, toCuaError } from '../errors.js';
import { isObservation, isScreenshot } from '../format.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const SURFACE_ROOT = path.resolve(here, '..', '..');
const CHILD = path.join(here, 'child.js');
const MAX_IMAGES = 64;
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
    this.imageSeq = 0;
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
    child.stderr.on('data', (d) => this.log(`sandbox: ${d.toString('utf8').trimEnd()}`));
    child.on('message', (msg) => this.#onMessage(gen, msg));
    child.on('exit', (code, signal) => {
      if (this.child !== child) return;
      this.child = null;
      this.ready = null;
      this.stateLost = true;
      this.log(`sandbox exited (${signal || code})`);
      if (this.cell) this.#complete(this.cell, { ok: false, error: `SandboxError: sandbox process exited (${signal || code}); REPL state was lost`, stateLost: true });
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
        this.log(`sandbox fatal: ${msg.error}`);
        break;
      case 'host_call':
        this.#hostCall(msg);
        break;
      case 'emit':
        this.#emit(msg);
        break;
      case 'done':
        if (this.cell && this.cell.id === msg.id) this.#complete(this.cell, msg);
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
      let params;
      try {
        params = JSON.parse(typeof msg.params === 'string' ? msg.params : '{}');
      } catch {
        throw new CuaError('invalid_params', 'params are not valid JSON');
      }
      const method = typeof msg.method === 'string' ? msg.method : '';
      const result = await this.host.call(method, params, { signal: cell.controller.signal, deadline: cell.deadline });
      if (cell.finished) throw new CuaError('cell_finished', 'the cell finished before this call completed');
      cell.calls.push(method);
      this.#reply(msg.callId, { ok: true, value: this.#toVm(method, result, cell) });
    } catch (err) {
      const e = toCuaError(err);
      this.#reply(msg.callId, { ok: false, error: e.toJSON() });
    } finally {
      cell.inflight--;
    }
  }

  #storeImage(shot) {
    const id = `img_${++this.imageSeq}`;
    this.images.set(id, { data: shot.data, mimeType: shot.mimeType, width: shot.width, height: shot.height });
    while (this.images.size > MAX_IMAGES) this.images.delete(this.images.keys().next().value);
    return { $image: id, mimeType: shot.mimeType, width: shot.width, height: shot.height };
  }

  #toVm(method, result, cell) {
    if (method === 'get_screenshot' && isScreenshot(result)) {
      const handle = this.#storeImage(result);
      cell.lastScreenshot = handle.$image;
      return handle;
    }
    if (isObservation(result)) {
      const copy = { ...result };
      if (isScreenshot(result.screenshot)) copy.screenshot = this.#storeImage(result.screenshot);
      cell.lastObservation = copy;
      return copy;
    }
    return result;
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
    if (msg.kind === 'log') {
      cell.logs.push({ level: String(payload.level || 'log'), text: String(payload.text ?? '') });
    } else if (msg.kind === 'image') {
      if (typeof payload.id === 'string' && this.images.has(payload.id)) cell.displayed.push(payload.id);
      else cell.logs.push({ level: 'warn', text: `display(): unknown or expired image handle` });
    }
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
    cell.resolve({
      ok: !!msg.ok,
      value: msg.value,
      error: msg.error,
      reason: msg.reason,
      stateLost: !!msg.stateLost,
      logs: cell.logs,
      calls: cell.calls,
      cancelledCalls: cell.cancelledCalls,
      observation: cell.lastObservation,
      images: this.#collectImages(cell),
    });
  }

  #collectImages(cell) {
    const ids = [...cell.displayed];
    if (cell.lastObservation?.screenshot?.$image) ids.push(cell.lastObservation.screenshot.$image);
    if (cell.lastScreenshot) ids.push(cell.lastScreenshot);
    const unique = [...new Set(ids)].slice(0, 8);
    return unique.map((id) => ({ id, ...this.images.get(id) })).filter((i) => i.data);
  }

  async eval({ code, timeoutMs, signal }) {
    if (typeof code !== 'string') throw new CuaError('invalid_params', 'code must be a string');
    if (this.cell) return { ok: false, error: 'BusyError: another cell is running', logs: [], images: [], calls: [], cancelledCalls: 0 };
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
        logs: [],
        displayed: [],
        calls: [],
        inflight: 0,
        cancelledCalls: 0,
        finished: false,
        lastObservation: null,
        lastScreenshot: null,
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
    this.stateLost = false;
    if (this.cell) this.#complete(this.cell, { ok: false, error: 'ResetError: REPL was reset', reason: 'reset', stateLost: true });
  }

  async close() {
    this.#kill('shutdown');
  }
}

// Client for the persistent Rust NDJSON daemon (CONTRACT.md).
//
// One request per line on stdin, one response per line on stdout. The daemon
// is a trusted host API: only HostApi talks to it. Cancellation and timeouts
// kill the daemon with SIGKILL so an action that is still in flight cannot
// land late; the next request respawns it and runs the onSpawn hook (HostApi
// uses that to replay approvals granted earlier in the same turn).
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CuaError } from './errors.js';

const here = path.dirname(fileURLToPath(import.meta.url));
export const DEFAULT_ENGINE = path.resolve(here, '..', '..', 'target', 'debug', 'linux-cua');
const MAX_LINE = 64 * 1024 * 1024;

// Refuse the owner's display. Accepts ":N", ":N.S", "host:N", "unix:N".
export function checkDisplay(display) {
  if (typeof display !== 'string' || !/^[A-Za-z0-9.\-_/]*:\d+(\.\d+)?$/.test(display)) {
    throw new CuaError('invalid_config', 'CUA_DISPLAY must be set to an explicit X display such as ":99"');
  }
  const num = display.slice(display.lastIndexOf(':') + 1).split('.')[0];
  if (Number(num) === 0) throw new CuaError('invalid_config', `refusing display ${display}: display :0 is not allowed`);
  return display;
}

export class EngineClient {
  // spawnArgs(): extra arguments computed at each (re)spawn (HostApi passes
  // the session's next element ID).
  constructor({ command = DEFAULT_ENGINE, args = [], spawnArgs, env = process.env, display, onSpawn, log = () => {} } = {}) {
    this.command = command;
    this.args = args;
    this.spawnArgs = spawnArgs;
    this.display = checkDisplay(display);
    this.env = { ...env, CUA_DISPLAY: this.display };
    this.onSpawn = onSpawn;
    this.log = log;
    this.proc = null;
    this.ready = null; // promise resolved when spawned and onSpawn finished
    this.pending = new Map();
    this.nextId = 1;
    this.generation = 0;
    this.restarts = 0;
  }

  get running() {
    return this.proc !== null;
  }

  async ensure() {
    if (this.ready) return this.ready;
    this.ready = this.#start().catch((err) => {
      this.ready = null;
      throw err;
    });
    return this.ready;
  }

  async #start() {
    if (this.exiting) await this.exiting;
    const gen = ++this.generation;
    const args = [...this.args, ...(this.spawnArgs ? this.spawnArgs() : [])];
    const proc = spawn(this.command, args, { stdio: ['pipe', 'pipe', 'pipe'], env: this.env });
    this.proc = proc;
    let resolveExit;
    proc.exitPromise = new Promise((r) => (resolveExit = r));
    await new Promise((resolve, reject) => {
      proc.once('spawn', resolve);
      proc.once('error', (err) => reject(new CuaError('engine_unavailable', `cannot start engine ${this.command}: ${err.message}`)));
    }).catch((err) => {
      this.proc = null;
      throw err;
    });
    proc.on('error', (err) => this.log(`engine error: ${err.message}`));
    proc.stderr.on('data', (chunk) => this.log(`engine: ${chunk.toString('utf8').trimEnd()}`));
    proc.stdin.on('error', () => {});
    const rl = createInterface({ input: proc.stdout, crlfDelay: Infinity });
    rl.on('line', (line) => this.#onLine(gen, line));
    proc.on('exit', (code, signal) => {
      resolveExit();
      if (this.proc === proc) {
        this.proc = null;
        this.ready = null;
      }
      this.#rejectAll(gen, new CuaError('engine_exited', `engine exited (${signal || code})`));
    });
    if (this.onSpawn) await this.onSpawn((method, params) => this.#send(method, params, {}));
  }

  #onLine(gen, line) {
    if (!line.trim()) return;
    let msg;
    try {
      if (line.length > MAX_LINE) throw new Error('line too long');
      msg = JSON.parse(line);
    } catch (err) {
      this.kill(`protocol violation on engine stdout: ${err.message}`);
      return;
    }
    const entry = msg && this.pending.get(msg.id);
    if (!entry || entry.gen !== gen) {
      this.log(`engine: dropping response for unknown id ${JSON.stringify(msg?.id)}`);
      return;
    }
    this.pending.delete(msg.id);
    entry.cleanup();
    if (msg.error) {
      const e = msg.error;
      const code = typeof e.code === 'string' ? e.code : String(e.code ?? 'engine_error');
      entry.reject(new CuaError(code, typeof e.message === 'string' ? e.message : 'engine error', e.data));
    } else if (Object.hasOwn(msg, 'result')) {
      entry.resolve(msg.result);
    } else {
      entry.reject(new CuaError('protocol_error', 'engine response has neither result nor error'));
    }
  }

  #rejectAll(gen, err) {
    for (const [id, entry] of this.pending) {
      if (entry.gen !== gen) continue;
      this.pending.delete(id);
      entry.cleanup();
      entry.reject(err);
    }
  }

  // Kill the daemon immediately (SIGKILL) so no in-flight action can complete.
  kill(reason = 'killed') {
    const proc = this.proc;
    if (!proc) return;
    this.log(`engine: killing (${reason})`);
    this.restarts++;
    this.proc = null;
    this.ready = null;
    this.exiting = proc.exitPromise;
    this.#rejectAll(this.generation, new CuaError('engine_killed', `engine terminated: ${reason}`));
    try {
      proc.kill('SIGKILL');
    } catch {}
  }

  async request(method, params = {}, { signal, deadline } = {}) {
    if (signal?.aborted) throw new CuaError('cancelled', `${method} cancelled before start`);
    await this.ensure();
    return this.#send(method, params, { signal, deadline });
  }

  #send(method, params, { signal, deadline }) {
    const proc = this.proc;
    if (!proc) return Promise.reject(new CuaError('engine_exited', 'engine is not running'));
    if (signal?.aborted) return Promise.reject(new CuaError('cancelled', `${method} cancelled before start`));
    const id = this.nextId++;
    const gen = this.generation;
    return new Promise((resolve, reject) => {
      let timer = null;
      const onAbort = () => {
        // The request may already be executing inside the daemon: kill it.
        this.pending.delete(id);
        cleanup();
        reject(new CuaError('cancelled', `${method} cancelled; engine restarted to prevent a late action`));
        this.kill(`${method} cancelled`);
      };
      const cleanup = () => {
        if (timer) clearTimeout(timer);
        signal?.removeEventListener('abort', onAbort);
      };
      this.pending.set(id, { gen, resolve, reject, cleanup });
      if (signal) signal.addEventListener('abort', onAbort, { once: true });
      if (deadline !== undefined) {
        const ms = deadline - Date.now();
        if (ms <= 0) {
          this.pending.delete(id);
          cleanup();
          reject(new CuaError('timeout', `${method} deadline already passed`));
          return;
        }
        timer = setTimeout(() => {
          this.pending.delete(id);
          cleanup();
          reject(new CuaError('timeout', `${method} timed out after ${ms} ms; engine restarted to prevent a late action`));
          this.kill(`${method} timed out`);
        }, ms);
      }
      proc.stdin.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }

  async close() {
    const proc = this.proc;
    this.kill('shutdown');
    if (proc) await proc.exitPromise;
  }
}

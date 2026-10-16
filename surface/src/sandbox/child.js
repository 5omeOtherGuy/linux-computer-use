// Sandbox process. Started by SandboxRunner with Node's permission model
// (`--permission`, read access to the surface package only; no write, no
// child processes, no workers, no addons, no WASI) and a scrubbed
// environment. Model code never runs in Node: it runs in a QuickJS
// WebAssembly VM whose only capabilities are the three native functions
// installed below. The VM has no filesystem, process, network, timer or
// module APIs. This process talks to the trusted host only over the IPC
// channel; the host revalidates every request.
import { getQuickJS } from 'quickjs-emscripten';
import { transformCell } from './transform.js';
import { preludeSource } from './prelude.js';

const MAX_PARAMS_CHARS = 2_000_000;
const MAX_LOG_CHARS = 200_000;
const MAX_EMITS = 2_000;
const MAX_IMAGE_CHARS = 32_000_000;

let rt = null;
let vm = null;
let fmtHandle = null;
let fmtErrorHandle = null;
let current = null; // active cell
let interruptAt = null;
let callSeq = 0;
const pending = new Map(); // callId -> { deferred, evalId, timer? }

const send = (msg) => {
  if (process.connected) process.send(msg);
};

function withBudget(ms, fn) {
  const saved = interruptAt;
  interruptAt = Date.now() + ms;
  try {
    return fn();
  } finally {
    interruptAt = saved;
  }
}

// Run queued VM jobs. Outside an active cell (or after an abort) jobs only
// get a small CPU budget, so stragglers cannot keep running between cells.
function pump(budgetMs = 50) {
  const run = () => rt.executePendingJobs().dispose();
  if (current && !current.abortReason) run();
  else withBudget(budgetMs, run);
}

function describe(handle, formatter) {
  return withBudget(250, () => {
    const r = vm.callFunction(formatter, vm.undefined, handle);
    if (r.error) {
      r.error.dispose();
      return '[unformattable value]';
    }
    const s = vm.typeof(r.value) === 'string' ? vm.getString(r.value) : String(vm.dump(r.value));
    r.value.dispose();
    return s;
  });
}

// Host results reach the VM as a JSON string. Image bytes (`$bytes`, base64
// from the engine) are decoded here, natively, and passed as an ArrayBuffer
// next to the JSON: decoding in VM JavaScript took ~0.4 s per megabyte.
function resultHandle(payload) {
  const v = payload?.value;
  if (!payload?.ok || !v || typeof v !== 'object' || typeof v.$bytes !== 'string') return vm.newString(JSON.stringify(payload));
  const bytes = Buffer.from(v.$bytes, 'base64');
  const obj = vm.newObject();
  vm.newString(JSON.stringify({ ok: true, value: { ...v, $bytes: true } })).consume((h) => vm.setProp(obj, 'json', h));
  vm.newArrayBuffer(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)).consume((h) => vm.setProp(obj, 'bytes', h));
  return obj;
}

function settleCall(callId, payload) {
  const entry = pending.get(callId);
  if (!entry) return;
  pending.delete(callId);
  if (entry.timer) clearTimeout(entry.timer);
  const h = payload === undefined ? vm.undefined : resultHandle(payload);
  entry.deferred.resolve(h);
  if (payload !== undefined) h.dispose();
  entry.deferred.dispose();
}

function failPending(evalId, code, message) {
  for (const [callId, entry] of [...pending]) {
    if (evalId === undefined || entry.evalId === evalId) {
      settleCall(callId, entry.kind === 'sleep' ? undefined : { ok: false, error: { code, message } });
    }
  }
}

function init({ memoryLimitMb = 128, stackKb = 1024 }) {
  return getQuickJS().then((QuickJS) => {
    rt = QuickJS.newRuntime();
    rt.setMemoryLimit(memoryLimitMb * 1024 * 1024);
    rt.setMaxStackSize(stackKb * 1024);
    rt.setInterruptHandler(() => interruptAt !== null && Date.now() > interruptAt);
    vm = rt.newContext();

    const hostCall = vm.newFunction('__cua_host_call', (methodH, paramsH, optsH) => {
      const deferred = vm.newPromise();
      const method = vm.typeof(methodH) === 'string' ? vm.getString(methodH) : '';
      const params = paramsH && vm.typeof(paramsH) === 'string' ? vm.getString(paramsH) : '{}';
      const opts = optsH && vm.typeof(optsH) === 'string' ? vm.getString(optsH) : '{}';
      const callId = ++callSeq;
      if (!current || current.abortReason) {
        const h = vm.newString(JSON.stringify({ ok: false, error: { code: 'no_active_cell', message: 'host calls are only allowed while a cell is running' } }));
        deferred.resolve(h);
        h.dispose();
      } else if (params.length > MAX_PARAMS_CHARS || opts.length > 1000) {
        const h = vm.newString(JSON.stringify({ ok: false, error: { code: 'invalid_params', message: 'params too large' } }));
        deferred.resolve(h);
        h.dispose();
      } else {
        pending.set(callId, { deferred, evalId: current.id, kind: 'host' });
        send({ type: 'host_call', evalId: current.id, callId, method, params, opts });
      }
      deferred.settled.then(() => pump());
      return deferred.handle;
    });

    const emit = vm.newFunction('__cua_emit', (kindH, payloadH, bytesH) => {
      if (!current) return vm.undefined;
      const kind = vm.getString(kindH);
      let payload = vm.getString(payloadH);
      if (kind === 'image' && bytesH && vm.typeof(bytesH) === 'object') {
        // Raw image bytes from model code: encoded natively.
        const meta = JSON.parse(payload);
        const data = vm.getArrayBuffer(bytesH).consume(({ value: u8 }) => (u8.length * 4 / 3 > MAX_IMAGE_CHARS ? null : Buffer.from(u8).toString('base64')));
        payload = JSON.stringify(data === null ? { mimeType: meta.mimeType, tooLarge: true } : { mimeType: meta.mimeType, data });
      }
      current.emits++;
      if (kind === 'image') current.imageChars += payload.length;
      else current.chars += payload.length;
      if (current.emits > MAX_EMITS || current.chars > MAX_LOG_CHARS || current.imageChars > MAX_IMAGE_CHARS) {
        if (!current.truncated) {
          current.truncated = true;
          send({ type: 'emit', evalId: current.id, kind: 'text', payload: JSON.stringify({ level: 'warn', text: '[output truncated]' }) });
        }
        return vm.undefined;
      }
      if (kind === 'text' || kind === 'image') send({ type: 'emit', evalId: current.id, kind, payload });
      return vm.undefined;
    });

    const sleep = vm.newFunction('__cua_sleep', (msH) => {
      const deferred = vm.newPromise();
      const ms = Math.max(0, Number(vm.getNumber(msH)) || 0);
      if (!current || current.abortReason) {
        deferred.resolve(vm.undefined);
      } else {
        const callId = ++callSeq;
        const wait = Math.min(ms, Math.max(0, current.deadline - Date.now()));
        const timer = setTimeout(() => settleCall(callId, undefined), wait);
        pending.set(callId, { deferred, evalId: current.id, kind: 'sleep', timer });
      }
      deferred.settled.then(() => pump());
      return deferred.handle;
    });

    for (const fn of [hostCall, emit, sleep]) {
      vm.setProp(vm.global, fn === hostCall ? '__cua_host_call' : fn === emit ? '__cua_emit' : '__cua_sleep', fn);
      fn.dispose();
    }
    const r = vm.evalCode(preludeSource(), 'prelude.js');
    if (r.error) {
      const msg = JSON.stringify(vm.dump(r.error));
      r.error.dispose();
      throw new Error(`prelude failed: ${msg}`);
    }
    r.value.dispose();
    fmtHandle = vm.getProp(vm.global, '__cua_fmt');
    fmtErrorHandle = vm.getProp(vm.global, '__cua_fmt_error');
  });
}

function finish(id, result) {
  if (!current || current.id !== id) return;
  const cell = current;
  clearTimeout(cell.timer);
  current = null;
  interruptAt = null;
  // Unawaited host calls or sleeps of this cell are released with an error;
  // any straggling continuations get a short CPU budget and cannot reach the
  // host because no cell is active any more.
  failPending(id, 'cell_finished', 'the cell finished before this call completed');
  pump(50);
  send({ type: 'done', id, ...result, reason: cell.abortReason || undefined });
}

function abort(id, reason) {
  if (!current || current.id !== id || current.abortReason) return;
  current.abortReason = reason;
  interruptAt = Date.now(); // interrupt anything that runs from here on
  failPending(id, reason, `cell ${reason}`);
  pump(0);
  finish(id, { ok: false, error: reason === 'timeout' ? 'TimeoutError: cell exceeded timeout_ms' : 'CancelledError: cell was cancelled' });
}

function startEval({ id, code, timeoutMs }) {
  if (current) {
    send({ type: 'done', id, ok: false, error: 'BusyError: another cell is running' });
    return;
  }
  let wrapped;
  try {
    wrapped = transformCell(code);
  } catch (err) {
    send({ type: 'done', id, ok: false, error: `SyntaxError: ${err.message}` });
    return;
  }
  current = { id, deadline: Date.now() + timeoutMs, emits: 0, chars: 0, imageChars: 0, truncated: false, abortReason: null, timer: null };
  interruptAt = current.deadline;
  current.timer = setTimeout(() => abort(id, 'timeout'), timeoutMs);
  const res = vm.evalCode(wrapped, `cell-${id}.js`);
  if (res.error) {
    const msg = describe(res.error, fmtErrorHandle);
    res.error.dispose();
    if (current && current.id === id && Date.now() > current.deadline) abort(id, 'timeout');
    else finish(id, { ok: false, error: msg });
    return;
  }
  const promise = vm.resolvePromise(res.value);
  res.value.dispose();
  promise.then((r) => {
    if (!current || current.id !== id) {
      r.dispose();
      return;
    }
    if (r.error) {
      const msg = describe(r.error, fmtErrorHandle);
      r.error.dispose();
      if (Date.now() > current.deadline) abort(id, 'timeout');
      else finish(id, { ok: false, error: msg });
    } else {
      const isUndef = vm.typeof(r.value) === 'undefined';
      const value = isUndef ? undefined : describe(r.value, fmtHandle);
      r.value.dispose();
      finish(id, { ok: true, value });
    }
  });
  pump();
}

process.on('message', async (msg) => {
  try {
    switch (msg?.type) {
      case 'init':
        await init(msg);
        send({ type: 'ready' });
        break;
      case 'eval':
        startEval(msg);
        break;
      case 'host_result':
        settleCall(msg.callId, msg.ok ? { ok: true, value: msg.value } : { ok: false, error: msg.error });
        break;
      case 'cancel':
        abort(msg.id, 'cancelled');
        break;
      default:
        break;
    }
  } catch (err) {
    send({ type: 'fatal', error: String(err?.stack || err) });
    process.exit(70);
  }
});
process.on('disconnect', () => process.exit(0));
// Anything that escapes (e.g. a QuickJS/WebAssembly abort inside a promise
// continuation) is reported to the host before the process exits, so the
// cause reaches the cell result and the server log.
for (const ev of ['uncaughtException', 'unhandledRejection']) {
  process.on(ev, (err) => {
    try {
      send({ type: 'fatal', error: `${ev}: ${String(err?.stack || err)}` });
    } finally {
      process.exitCode = 70;
      setImmediate(() => process.exit(70));
    }
  });
}

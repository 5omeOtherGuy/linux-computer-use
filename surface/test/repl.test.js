// REPL semantics and sandbox limits, using SandboxRunner with a stub host.
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { transformCell } from '../src/sandbox/transform.js';
import { SandboxRunner } from '../src/sandbox/runner.js';

describe('transformCell', () => {
  test('keeps line numbers and returns the last expression', () => {
    const out = transformCell('let a = 1\nconst b = 2\na + b');
    assert.equal(out.split('\n').length, 4); // 3 source lines + closing line
    assert.match(out, /return \(a \+ b\)/);
  });

  test('rejects syntax errors with position', () => {
    assert.throws(() => transformCell('let = ;'), /SyntaxError|Unexpected/);
  });
});

describe('SandboxRunner', () => {
  const calls = [];
  const host = {
    methods: ['list_windows', 'get_screenshot'],
    async call(method, params) {
      calls.push({ method, params });
      if (method === 'get_screenshot') return { data: 'iVBORw0KGgo=', mimeType: 'image/png', width: 1, height: 1 };
      return [{ window_id: 1 }];
    },
  };
  let r;
  before(() => {
    r = new SandboxRunner({ host, memoryLimitMb: 32 });
  });
  after(() => r.close());

  test('sandbox process runs under the Node permission model with an empty environment', { skip: process.platform !== 'linux' }, async () => {
    await r.eval({ code: '0', timeoutMs: 5000 });
    const { readFileSync } = await import('node:fs');
    const argv = readFileSync(`/proc/${r.child.pid}/cmdline`, 'utf8').split('\0');
    assert.ok(argv.includes('--permission'));
    assert.ok(argv.some((a) => a.startsWith('--allow-fs-read=')));
    assert.ok(!argv.some((a) => /allow-(fs-write|child-process|worker|addons|wasi)/.test(a)));
    const env = readFileSync(`/proc/${r.child.pid}/environ`, 'utf8').split('\0').filter(Boolean).map((e) => e.split('=')[0]);
    assert.deepEqual(env.filter((k) => !k.startsWith('NODE_CHANNEL')), []);
  });

  test('destructuring, classes, functions and await persist', async () => {
    let res = await r.eval({ code: 'const { a, b: [c = 5] } = { a: 1, b: [] }; var v; let u; async function f() { return (await cua.list_windows()).length }', timeoutMs: 5000 });
    assert.equal(res.ok, true, res.error);
    res = await r.eval({ code: '[a, c, typeof v, typeof u, await f()].join()', timeoutMs: 5000 });
    assert.equal(res.value, '1,5,undefined,undefined,1');
  });

  test('completion value of trailing try/if blocks', async () => {
    const res = await r.eval({ code: 'if (a === 1) { "yes" } else { "no" }', timeoutMs: 5000 });
    assert.equal(res.value, 'yes');
  });

  test('errors carry code and line', async () => {
    const res = await r.eval({ code: '\n\nnull.x', timeoutMs: 5000 });
    assert.equal(res.ok, false);
    assert.match(res.error, /TypeError/);
    assert.match(res.error, /cell-\d+\.js:3/);
  });

  test('images are host-side handles that display() can show', async () => {
    const res = await r.eval({ code: 'const s = await cua.get_screenshot({window_id: 1}); display(s); Object.keys(s).sort().join(",")', timeoutMs: 5000 });
    assert.equal(res.value, '$image,height,mimeType,width');
    assert.equal(res.images.length, 1);
    assert.equal(res.images[0].data, 'iVBORw0KGgo=');
  });

  test('memory limit stops runaway allocation without killing the process', async () => {
    let res = await r.eval({ code: 'let big = []; while (true) big.push({ n: big.length })', timeoutMs: 20_000 });
    assert.equal(res.ok, false);
    assert.match(res.error, /out of memory/i);
    res = await r.eval({ code: 'big = null; a', timeoutMs: 5000 });
    assert.equal(res.value, '1');
  });

  test('an unresponsive sandbox is killed at the deadline and reports lost state', async () => {
    let res = await r.eval({ code: 'let marker = 1; marker', timeoutMs: 5000 });
    assert.equal(res.value, '1');
    // Simulate a sandbox stuck in native code: freeze the process we started.
    process.kill(r.child.pid, 'SIGSTOP');
    const t0 = Date.now();
    res = await r.eval({ code: '1', timeoutMs: 300 });
    assert.equal(res.ok, false);
    assert.equal(res.reason, 'timeout');
    assert.equal(res.stateLost, true);
    assert.ok(Date.now() - t0 < 4000);
    res = await r.eval({ code: 'typeof marker', timeoutMs: 5000 });
    assert.equal(res.value, 'undefined', 'fresh sandbox after kill');
  });

  test('console output is captured and bounded', async () => {
    const res = await r.eval({ code: 'for (let i = 0; i < 5000; i++) console.log("line", i)', timeoutMs: 10_000 });
    assert.equal(res.ok, true);
    assert.ok(res.logs.length <= 2001);
    assert.equal(res.logs.at(-1).text, '[output truncated]');
  });

  test('host calls are only possible while a cell runs', async () => {
    // A promise chain that would call the host after the cell ended.
    const before = calls.length;
    let res = await r.eval({ code: 'globalThis.later = Promise.resolve().then(() => sleep(50)).then(() => cua.list_windows()).then(() => "ran", (e) => e.code); "started"', timeoutMs: 5000 });
    assert.equal(res.value, 'started');
    res = await r.eval({ code: 'await later', timeoutMs: 5000 });
    assert.equal(res.value, 'no_active_cell');
    assert.equal(calls.length, before);
  });
});

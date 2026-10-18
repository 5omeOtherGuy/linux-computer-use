// benchmarks/run.mjs refuses the full-access agent-desktop controller without
// an explicit flag, before it starts anything. Stubs on PATH record any call.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const RUN = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../benchmarks/run.mjs');

for (const agent of ['codex', 'claude']) test(`the agent-desktop controller with ${agent} needs --allow-full-access`, (t) => {
  const dir = mkdtempSync(path.join(tmpdir(), 'cua-bench-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const marker = path.join(dir, 'started');
  for (const name of ['codex', 'claude', 'agent-desktop']) {
    writeFileSync(path.join(dir, name), `#!/bin/sh\necho ${name} >> ${marker}\nexit 1\n`);
    chmodSync(path.join(dir, name), 0o755);
  }
  const r = spawnSync(process.execPath, [RUN, '--controller', 'agent-desktop', '--agent', agent, '--model', 'm'], {
    env: { PATH: `${dir}:${process.env.PATH}`, HOME: dir, BENCH_ROOT: dir, CUA_AGENT_DESKTOP: path.join(dir, 'agent-desktop') },
    encoding: 'utf8',
  });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /full access[^]*--allow-full-access/);
  assert.equal(existsSync(marker), false, 'nothing was started');
  assert.equal(existsSync(path.join(dir, 'results.jsonl')), false);
});

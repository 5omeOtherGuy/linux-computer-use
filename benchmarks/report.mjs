#!/usr/bin/env node
// Summarises $BENCH_ROOT/results.jsonl by label/task/agent/model/effort:
// pass rate and medians of wall time, tool calls and tool time.
//   node benchmarks/report.mjs [--label L]
import { readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';

const { values: o } = parseArgs({ options: { label: { type: 'string' } } });
const root = process.env.BENCH_ROOT || '/data/scratch/linux-cua-bench';
const rows = readFileSync(`${root}/results.jsonl`, 'utf8').trim().split('\n').map((l) => JSON.parse(l))
  .filter((r) => !o.label || r.label === o.label);
const median = (xs) => {
  const v = xs.filter((x) => typeof x === 'number').sort((a, b) => a - b);
  if (!v.length) return null;
  const m = v.length >> 1;
  return v.length % 2 ? v[m] : (v[m - 1] + v[m]) / 2;
};
const groups = new Map();
for (const r of rows) {
  const k = [r.label, r.task, r.agent, r.model, r.effort ?? '-', r.tier ?? '-', r.surface].join(' | ');
  if (!groups.has(k)) groups.set(k, []);
  groups.get(k).push(r);
}
const s = (ms) => (ms == null ? '-' : (ms / 1000).toFixed(1));
console.log('label | task | agent | model | effort | tier | surface | pass | median wall s (passed) | median tools | median tool s');
for (const [k, rs] of groups) {
  const ok = rs.filter((r) => r.success);
  console.log(`${k} | ${ok.length}/${rs.length} | ${s(median(ok.map((r) => r.wall_ms)))} | ${median(ok.map((r) => r.tool_calls)) ?? '-'} | ${s(median(ok.map((r) => r.tool_ms)))}`);
}

#!/usr/bin/env node
// Summarises $BENCH_ROOT/results.jsonl by label, task, controller, agent,
// model and effort: runs, passes, and over the passed runs the median and
// minimum wall time, the median and minimum time from the first tool call
// (wall - first_tool_ms, i.e. without the model's initial think), the median
// tool calls and the median tool seconds. Rows before the controller field
// existed ran the cua controller.
//   node benchmarks/report.mjs [--label L] [--task T]
import { readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';

const { values: o } = parseArgs({ options: { label: { type: 'string' }, task: { type: 'string' } } });
const root = process.env.BENCH_ROOT || '/data/scratch/linux-cua-bench';
const rows = readFileSync(`${root}/results.jsonl`, 'utf8').trim().split('\n').map((l) => JSON.parse(l))
  .filter((r) => (!o.label || r.label === o.label) && (!o.task || r.task === o.task));
const nums = (xs) => xs.filter((x) => typeof x === 'number').sort((a, b) => a - b);
const median = (xs) => {
  const v = nums(xs);
  if (!v.length) return null;
  const m = v.length >> 1;
  return v.length % 2 ? v[m] : (v[m - 1] + v[m]) / 2;
};
const min = (xs) => nums(xs)[0] ?? null;
const groups = new Map();
for (const r of rows) {
  const k = [r.label || '-', r.task, r.controller ?? 'cua', r.agent, r.model, r.effort ?? '-'].join(' | ');
  if (!groups.has(k)) groups.set(k, []);
  groups.get(k).push(r);
}
const s = (ms) => (ms == null ? '-' : (ms / 1000).toFixed(1));
console.log('label | task | controller | agent | model | effort | n | pass | wall s median | wall s min | from first tool s median | from first tool s min | tool calls median | tool s median');
for (const [k, rs] of groups) {
  const ok = rs.filter((r) => r.success);
  const fromFirst = ok.map((r) => (r.first_tool_ms == null ? null : r.wall_ms - r.first_tool_ms));
  console.log([k, rs.length, ok.length, s(median(ok.map((r) => r.wall_ms))), s(min(ok.map((r) => r.wall_ms))), s(median(fromFirst)), s(min(fromFirst)),
    median(ok.map((r) => r.tool_calls)) ?? '-', s(median(ok.map((r) => r.tool_ms)))].join(' | '));
}

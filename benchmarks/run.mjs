#!/usr/bin/env node
// End-to-end agent benchmark: a real model (GPT through `codex exec`, Claude
// through `claude -p`) drives real applications on a fresh private desktop
// through the linux-cua MCP server. Wall time covers model inference, tool
// round trips, settling and recovery; the outcome is verified from disk.
//
// The standard benchmark (the defaults): GPT-6.1-sol at medium effort in
// Codex draws and saves a picture in GNOME Drawing (task `paint`).
//
//   node benchmarks/run.mjs --label <change> [--iterations N] [--record]
//   node benchmarks/run.mjs --task reimburse --agent codex --model gpt-6.1-sol --effort low
//   node benchmarks/run.mjs --task xed-note --agent claude --model claude-opus-5-5 --record
//
// Results append to $BENCH_ROOT/results.jsonl (default /data/scratch/linux-cua-bench);
// `node benchmarks/report.mjs` summarises them per label.
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, writeFile, appendFile } from 'node:fs/promises';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { TASKS } from './tasks.mjs';

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const { values: o } = parseArgs({
  options: {
    task: { type: 'string', default: 'paint' },
    agent: { type: 'string', default: 'codex' },
    model: { type: 'string', default: 'gpt-6.1-sol' },
    effort: { type: 'string', default: 'medium' },
    tier: { type: 'string' },
    surface: { type: 'string', default: 'js' },
    iterations: { type: 'string', default: '1' },
    timeout: { type: 'string', default: '600' },
    record: { type: 'boolean', default: false },
    engine: { type: 'string', default: process.env.CUA_ENGINE || '/data/scratch/linux-cua-build/target/release/linux-cua' },
    'mcp-entry': { type: 'string', default: join(repo, 'surface/bin/linux-cua-mcp.js') },
    label: { type: 'string', default: '' },
  },
});
const task = TASKS[o.task];
if (!task || !['codex', 'claude'].includes(o.agent) || !o.model) {
  throw Error(`Usage: run.mjs --task ${Object.keys(TASKS).join('|')} --agent codex|claude --model MODEL [--effort E] [--tier T] [--surface js|json|all] [--iterations N] [--record]`);
}
const benchRoot = process.env.BENCH_ROOT || '/data/scratch/linux-cua-bench';
const GEOMETRY = '1440x900';

// Apps inherit the caller's cwd as their default save/open folder, so launch from the run's output dir.
let adCwd = process.cwd();
const ad = (...args) => execFileSync('agent-desktop', args, { encoding: 'utf8', timeout: 120_000, cwd: adCwd }).trim();

function workspaceEnv(ws) {
  const env = {};
  for (const line of ad('env', ws).split('\n')) {
    const m = line.match(/^(?:export\s+)?([A-Z_][A-Z0-9_]*)=(.*)$/);
    if (m) env[m[1]] = m[2].replace(/^'(.*)'$/s, '$1').replace(/^"(.*)"$/s, '$1');
  }
  if (!env.DISPLAY || env.DISPLAY.match(/:0(\.\d+)?$/)) throw Error(`refusing workspace display ${env.DISPLAY}`);
  return env;
}

// Tools offered to the model for each surface variant.
const SURFACES = { js: ['js', 'js_reset'], json: null, all: null };

function agentCommand({ prompt, runDir, mcpEnv }) {
  const entry = o['mcp-entry'];
  if (o.agent === 'codex') {
    const toml = (v) => JSON.stringify(v);
    const envToml = `{${Object.entries(mcpEnv).map(([k, v]) => `${k}=${toml(v)}`).join(',')}}`;
    const args = ['exec', '--json', '--skip-git-repo-check', '--ephemeral', '-s', 'read-only', '-C', runDir, '-m', o.model,
      '-c', 'mcp_servers.cua.command="node"', '-c', `mcp_servers.cua.args=[${toml(entry)}]`, '-c', `mcp_servers.cua.env=${envToml}`,
      '-c', 'mcp_servers.cua.startup_timeout_sec=60', '-c', 'mcp_servers.cua.tool_timeout_sec=180',
      // Same exposure as OpenAI's bundled cua_repl: direct tools, never deferred behind tool search.
      '-c', 'mcp_servers.cua.omit_tools_from=["code_mode","deferred"]',
      // Headless exec never prompts; the trusted operator pre-approves this server (apps are scoped by CUA_ALLOW_APPS).
      '-c', 'mcp_servers.cua.default_tools_approval_mode="approve"'];
    if (SURFACES[o.surface]) args.push('-c', `mcp_servers.cua.enabled_tools=${toml(SURFACES[o.surface])}`);
    if (o.surface === 'json') args.push('-c', 'mcp_servers.cua.disabled_tools=["js","js_reset","turn_ended"]');
    if (o.effort) args.push('-c', `model_reasoning_effort=${toml(o.effort)}`);
    if (o.tier) args.push('-c', `service_tier=${toml(o.tier)}`);
    args.push(prompt);
    return ['codex', args];
  }
  const cfg = join(runDir, 'mcp.json');
  execFileSync('bash', ['-c', 'cat > "$1"', '_', cfg], { input: JSON.stringify({ mcpServers: { cua: { command: 'node', args: [entry], env: mcpEnv } } }) });
  const allowed = SURFACES[o.surface] ? SURFACES[o.surface].map((t) => `mcp__cua__${t}`) : ['mcp__cua__*'];
  const args = ['-p', '--output-format', 'stream-json', '--verbose', '--model', o.model, '--mcp-config', cfg, '--strict-mcp-config',
    '--tools', '', '--allowedTools', ...allowed, '--no-session-persistence'];
  if (o.surface === 'json') args.push('--disallowedTools', 'mcp__cua__js', 'mcp__cua__js_reset');
  if (o.effort) args.push('--effort', o.effort);
  args.push('--', prompt);
  return ['claude', args];
}

// Turns the agent's JSON event stream (timestamped on arrival) into metrics.
function summarize(events) {
  const m = { tool_calls: 0, tool_errors: 0, non_cua_tools: [], tool_ms: 0, first_tool_ms: null, last_tool_ms: null, usage: null, final_message: '' };
  const open = new Map();
  for (const { t, e } of events) {
    if (o.agent === 'codex') {
      const it = e.item;
      if (e.type === 'item.started' && it) open.set(it.id, t);
      if (e.type === 'item.completed' && it) {
        if (it.type === 'mcp_tool_call') {
          m.tool_calls++;
          if (it.status === 'failed' || it.error) m.tool_errors++;
          const s = open.get(it.id) ?? t;
          m.tool_ms += t - s;
          m.first_tool_ms ??= s;
          m.last_tool_ms = t;
        } else if (['command_execution', 'file_change', 'web_search'].includes(it.type)) m.non_cua_tools.push(it.type);
        if (it.type === 'agent_message') m.final_message = it.text;
      }
      if (e.type === 'turn.completed') m.usage = e.usage;
    } else {
      const content = e.message?.content;
      if (e.type === 'assistant' && Array.isArray(content)) {
        for (const c of content) {
          if (c.type === 'tool_use') {
            open.set(c.id, t);
            m.tool_calls++;
            m.first_tool_ms ??= t;
            if (!c.name.startsWith('mcp__cua__')) m.non_cua_tools.push(c.name);
          }
          if (c.type === 'text') m.final_message = c.text;
        }
      }
      if (e.type === 'user' && Array.isArray(content)) {
        for (const c of content) {
          if (c.type === 'tool_result' && open.has(c.tool_use_id)) {
            m.tool_ms += t - open.get(c.tool_use_id);
            m.last_tool_ms = t;
            if (c.is_error) m.tool_errors++;
          }
        }
      }
      if (e.type === 'result') m.usage = { ...e.usage, total_cost_usd: e.total_cost_usd, num_turns: e.num_turns };
    }
  }
  return m;
}

async function runOnce(iteration) {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const runDir = join(benchRoot, 'runs', `${stamp}-${task.id}-${o.agent}-${iteration}`);
  const out = join(runDir, 'out');
  await mkdir(out, { recursive: true });
  adCwd = out;
  const ws = ad('new', '--label', `linux-cua bench ${task.id}`, '--geometry', GEOMETRY, '--a11y', '--idle', '30');
  let recorder = null;
  const result = { schema: 1, label: o.label, task: task.id, agent: o.agent, model: o.model, effort: o.effort ?? null, tier: o.tier ?? null, surface: o.surface, iteration, run_dir: runDir, started: new Date().toISOString() };
  try {
    const env = workspaceEnv(ws);
    const setupStart = performance.now();
    for (const app of task.apps) {
      ad('run', ws, '--', ...app.cmd.map((a) => a.replace('{out}', out)));
      ad('wait', ws, '--class', app.waitClass, '--timeout', '40');
    }
    result.setup_ms = performance.now() - setupStart;
    if (o.record) {
      recorder = spawn('ffmpeg', ['-loglevel', 'error', '-f', 'x11grab', '-video_size', GEOMETRY, '-framerate', '15', '-i', env.DISPLAY,
        '-c:v', 'libx264', '-preset', 'ultrafast', '-crf', '30', '-pix_fmt', 'yuv420p', join(runDir, 'video.mp4')],
      { env: { ...process.env, ...env }, stdio: ['pipe', 'ignore', 'inherit'] });
    }
    const mcpEnv = {
      CUA_ENGINE: o.engine, CUA_DISPLAY: env.DISPLAY, XAUTHORITY: env.XAUTHORITY,
      DBUS_SESSION_BUS_ADDRESS: env.DBUS_SESSION_BUS_ADDRESS, CUA_ALLOW_APPS: task.allowApps.join(','),
      ...(env.AT_SPI_BUS_ADDRESS ? { AT_SPI_BUS_ADDRESS: env.AT_SPI_BUS_ADDRESS } : {}),
    };
    const prompt = `${task.prompt(out)}\n\nThe applications are already open on the desktop. Use only the cua computer-use tools for every interaction with them.`;
    await writeFile(join(runDir, 'prompt.txt'), prompt);
    const [cmd, args] = agentCommand({ prompt, runDir, mcpEnv });
    await writeFile(join(runDir, 'command.json'), JSON.stringify([cmd, ...args], null, 1));
    const events = [];
    const t0 = performance.now();
    const child = spawn(cmd, args, { cwd: runDir, stdio: ['ignore', 'pipe', 'pipe'] });
    let buf = '';
    let stderr = '';
    child.stdout.on('data', (d) => {
      buf += d;
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 1);
        if (!line.trim()) continue;
        const t = performance.now() - t0;
        try { events.push({ t, e: JSON.parse(line) }); } catch { /* non-JSON noise */ }
        appendFile(join(runDir, 'events.jsonl'), JSON.stringify({ t: Math.round(t), ...JSON.parse(line) }) + '\n').catch(() => {});
      }
    });
    child.stderr.on('data', (d) => { stderr += d; });
    const killer = setTimeout(() => child.kill('SIGTERM'), Number(o.timeout) * 1000);
    const code = await new Promise((r) => child.on('exit', (c) => r(c)));
    clearTimeout(killer);
    result.wall_ms = performance.now() - t0;
    result.exit_code = code;
    await writeFile(join(runDir, 'agent.stderr'), stderr);
    Object.assign(result, summarize(events));
    if (result.first_tool_ms !== null) result.active_ms = result.last_tool_ms - result.first_tool_ms;
    result.model_ms = result.wall_ms - result.tool_ms;
    result.verify = await task.verify(out);
    result.success = result.verify.success && code === 0 && result.non_cua_tools.length === 0;
  } catch (err) {
    result.error = String(err?.message || err);
    result.success = false;
  } finally {
    if (recorder) {
      recorder.stdin.end('q');
      await new Promise((r) => recorder.on('exit', r));
    }
    try { ad('close', ws); } catch (err) { result.close_error = String(err.message); }
  }
  await mkdir(benchRoot, { recursive: true });
  await appendFile(join(benchRoot, 'results.jsonl'), JSON.stringify(result) + '\n');
  await writeFile(join(runDir, 'result.json'), JSON.stringify(result, null, 2));
  const s = (ms) => (ms == null ? '-' : (ms / 1000).toFixed(1) + 's');
  console.log(`${task.id} ${o.agent}/${o.model}${o.effort ? '/' + o.effort : ''} #${iteration}: ${result.success ? 'PASS' : 'FAIL'} wall ${s(result.wall_ms)} active ${s(result.active_ms)} tools ${result.tool_calls ?? '-'} (${s(result.tool_ms)}) errors ${result.tool_errors ?? '-'} ${result.error ?? ''} -> ${runDir}`);
  return result;
}

for (let i = 0; i < Number(o.iterations); i++) await runOnce(i);

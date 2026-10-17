#!/usr/bin/env node
// End-to-end agent benchmark: a real model (GPT through `codex exec`, Claude
// through `claude -p`) drives real applications on a fresh private desktop.
// Wall time covers model inference, tool round trips, settling and recovery;
// the outcome is verified from disk.
//
// Two controllers run under identical conditions (same desktop, apps, task
// prompt, model and verification):
//   cua            the linux-cua MCP server (`js` REPL tool); the default
//   agent-desktop  a shell restricted to `agent-desktop` commands on the
//                  benchmark's workspace, with the agent-desktop skill text in
//                  the prompt
//
// The standard benchmark (the defaults): GPT-6.1-sol at medium effort in
// Codex draws and saves a picture in GNOME Drawing (task `paint`).
//
//   node benchmarks/run.mjs --label <change> [--iterations N] [--record]
//   node benchmarks/run.mjs --task reimburse --agent codex --model gpt-6.1-sol --effort low
//   node benchmarks/run.mjs --task xed-note --agent claude --model claude-opus-5-5 --record
//   node benchmarks/run.mjs --task webform --controller agent-desktop --agent claude --model claude-opus-5-5 --effort low
//
// Results append to $BENCH_ROOT/results.jsonl (default /data/scratch/linux-cua-bench);
// `node benchmarks/report.mjs` summarises them.
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, writeFile, appendFile, readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { createServer } from 'node:net';
import { homedir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { TASKS } from './tasks.mjs';
import { checkCommand } from './shell-policy.mjs';
import { agentDesktop } from '../surface/src/workspace.js';

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const { values: o } = parseArgs({
  options: {
    task: { type: 'string', default: 'paint' },
    controller: { type: 'string', default: 'cua' },
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
    'skill-dir': { type: 'string', default: join(repo, 'desktop/skill') },
    label: { type: 'string', default: '' },
  },
});
const task = TASKS[o.task];
if (!task || !['codex', 'claude'].includes(o.agent) || !['cua', 'agent-desktop'].includes(o.controller) || !o.model) {
  throw Error(`Usage: run.mjs --task ${Object.keys(TASKS).join('|')} [--controller cua|agent-desktop] --agent codex|claude --model MODEL [--effort E] [--tier T] [--surface js|json|all] [--iterations N] [--record]`);
}
const AD = o.controller === 'agent-desktop';
const benchRoot = process.env.BENCH_ROOT || '/data/scratch/linux-cua-bench';
const GEOMETRY = '1440x900';

// Apps inherit the caller's cwd as their default save/open folder, so launch from the run's output dir.
let adCwd = process.cwd();
const ad = (...args) => execFileSync(agentDesktop(process.env), args, { encoding: 'utf8', timeout: 120_000, cwd: adCwd }).trim();

function workspaceEnv(ws) {
  const env = {};
  for (const line of ad('env', ws).split('\n')) {
    const m = line.match(/^(?:export\s+)?([A-Z_][A-Z0-9_]*)=(.*)$/);
    if (m) env[m[1]] = m[2].replace(/^'(.*)'$/s, '$1').replace(/^"(.*)"$/s, '$1');
  }
  if (!env.DISPLAY || env.DISPLAY.match(/:0(\.\d+)?$/)) throw Error(`refusing workspace display ${env.DISPLAY}`);
  return env;
}

const freePort = () => new Promise((ok, fail) => {
  const s = createServer().once('error', fail).listen(0, '127.0.0.1', () => {
    const { port } = s.address();
    s.close(() => ok(port));
  });
});

// Tools offered to the model for each surface variant.
const SURFACES = { js: ['js', 'js_reset'], json: null, all: null };
// Every tool the cua MCP server lists besides js/js_reset (Claude's init event,
// 2026-10-16). Claude lists all MCP tools unless they are disallowed; Codex
// gets the same js/js_reset-only view through enabled_tools. The init event's
// tool list is recorded as `offered_tools`, so a newly added server tool shows up there.
const CUA_NON_JS_TOOLS = ['click', 'clipboard_read', 'clipboard_release', 'clipboard_write', 'drag', 'get_app_state', 'get_ax_state',
  'get_screenshot', 'launch_app', 'list_apps', 'list_windows', 'mouse_move', 'paste', 'perform_secondary_action', 'press_key',
  'scroll', 'set_value', 'turn_ended', 'type_text'];

// The agent-desktop skill as an agent reads it: SKILL.md plus its references.
const SKILL_FILES = ['SKILL.md', 'references/native-control.md', 'references/browser-control.md', 'references/fast-input.md'];
async function skillDocs() {
  const files = await Promise.all(SKILL_FILES.map(async (f) => ({ f, text: await readFile(join(o['skill-dir'], f), 'utf8') })));
  return {
    text: files.map(({ f, text }) => `<file path="${f}">\n${text.trim()}\n</file>`).join('\n\n'),
    sha256: Object.fromEntries(files.map(({ f, text }) => [f, createHash('sha256').update(text).digest('hex')])),
  };
}

function agentCommand({ prompt, runDir, mcpEnv, shotsDir }) {
  const entry = o['mcp-entry'];
  if (o.agent === 'codex') {
    const toml = (v) => JSON.stringify(v);
    const args = ['exec', '--json', '--skip-git-repo-check', '--ephemeral', '-m', o.model];
    if (AD) {
      // agent-desktop must see the workspace processes: inside Codex's
      // sandbox (read-only or workspace-write) other PIDs are hidden, so its
      // garbage collector takes every workspace on the machine for dead and
      // closes them (observed 2026-10-17). It therefore runs unsandboxed;
      // the command rule is checked afterwards (checkCommand).
      args.push('-s', 'danger-full-access', '-c', 'approval_policy="never"', '-C', join(runDir, 'agent'));
    } else {
      const envToml = `{${Object.entries(mcpEnv).map(([k, v]) => `${k}=${toml(v)}`).join(',')}}`;
      args.push('-s', 'read-only', '-C', runDir,
        '-c', 'mcp_servers.cua.command="node"', '-c', `mcp_servers.cua.args=[${toml(entry)}]`, '-c', `mcp_servers.cua.env=${envToml}`,
        '-c', 'mcp_servers.cua.startup_timeout_sec=60', '-c', 'mcp_servers.cua.tool_timeout_sec=180',
        // Same exposure as OpenAI's bundled cua_repl: direct tools, never deferred behind tool search.
        '-c', 'mcp_servers.cua.omit_tools_from=["code_mode","deferred"]',
        // Headless exec never prompts; the trusted operator pre-approves this server (apps are scoped by CUA_ALLOW_APPS).
        '-c', 'mcp_servers.cua.default_tools_approval_mode="approve"');
      if (SURFACES[o.surface]) args.push('-c', `mcp_servers.cua.enabled_tools=${toml(SURFACES[o.surface])}`);
      if (o.surface === 'json') args.push('-c', 'mcp_servers.cua.disabled_tools=["js","js_reset","turn_ended"]');
    }
    if (o.effort) args.push('-c', `model_reasoning_effort=${toml(o.effort)}`);
    if (o.tier) args.push('-c', `service_tier=${toml(o.tier)}`);
    args.push(prompt);
    return ['codex', args];
  }
  const args = ['-p', '--output-format', 'stream-json', '--verbose', '--model', o.model, '--no-session-persistence'];
  if (AD) {
    // Bash only for agent-desktop commands and Read only for its screenshots;
    // in -p mode with dontAsk anything else is denied (and still counted as a violation).
    args.push('--strict-mcp-config', '--tools', 'Bash,Read', '--permission-mode', 'dontAsk',
      '--allowedTools', 'Bash(agent-desktop:*)', `Read(/${shotsDir}/**)`,
      '--disallowedTools', 'Bash(agent-desktop new:*)', 'Bash(agent-desktop close:*)', 'Bash(agent-desktop view:*)');
  } else {
    const cfg = join(runDir, 'mcp.json');
    execFileSync('bash', ['-c', 'cat > "$1"', '_', cfg], { input: JSON.stringify({ mcpServers: { cua: { command: 'node', args: [entry], env: mcpEnv } } }) });
    const allowed = SURFACES[o.surface] ? SURFACES[o.surface].map((t) => `mcp__cua__${t}`) : ['mcp__cua__*'];
    args.push('--mcp-config', cfg, '--strict-mcp-config', '--tools', '', '--allowedTools', ...allowed);
    const disallowed = o.surface === 'js' ? CUA_NON_JS_TOOLS : o.surface === 'json' ? ['js', 'js_reset'] : [];
    if (disallowed.length) args.push('--disallowedTools', ...disallowed.map((t) => `mcp__cua__${t}`));
  }
  if (o.effort) args.push('--effort', o.effort);
  args.push('--', prompt);
  return ['claude', args];
}

// Turns the agent's JSON event stream (timestamped on arrival) into metrics.
// `violations` lists every tool use outside the controller's rules.
function summarize(events, { ws, runApps, shotsDir }) {
  const m = { tool_calls: 0, tool_errors: 0, violations: [], tool_ms: 0, first_tool_ms: null, last_tool_ms: null, usage: null, final_message: '', offered_tools: null };
  const commands = [];
  const open = new Map();
  const toolUse = (s, t, error) => {
    m.tool_calls++;
    if (error) m.tool_errors++;
    m.tool_ms += t - s;
    m.first_tool_ms = m.first_tool_ms === null ? s : Math.min(m.first_tool_ms, s);
    m.last_tool_ms = t;
  };
  for (const { t, e } of events) {
    if (o.agent === 'codex') {
      const it = e.item;
      if (e.type === 'item.started' && it) open.set(it.id, t);
      if (e.type === 'item.completed' && it) {
        const s = open.get(it.id) ?? t;
        if (it.type === 'mcp_tool_call' && !AD) toolUse(s, t, it.status === 'failed' || it.error);
        else if (it.type === 'command_execution' && AD) {
          toolUse(s, t, it.exit_code !== 0);
          commands.push({ command: it.command, ms: Math.round(t - s), exit_code: it.exit_code });
          m.violations.push(...checkCommand(it.command, ws, runApps));
        } else if (!['agent_message', 'reasoning', 'todo_list', 'error'].includes(it.type)) {
          toolUse(s, t, it.status === 'failed');
          m.violations.push(`tool ${it.type}${it.command ? `: ${String(it.command).slice(0, 200)}` : ''}`);
        }
        if (it.type === 'agent_message') m.final_message = it.text;
      }
      if (e.type === 'turn.completed') m.usage = e.usage;
    } else {
      const content = e.message?.content;
      if (e.type === 'system' && e.subtype === 'init') m.offered_tools = e.tools;
      if (e.type === 'assistant' && Array.isArray(content)) {
        for (const c of content) {
          if (c.type === 'tool_use') {
            open.set(c.id, { t, c });
            if (!AD) {
              if (!c.name.startsWith('mcp__cua__')) m.violations.push(`tool ${c.name}`);
            } else if (c.name === 'Bash') {
              m.violations.push(...checkCommand(String(c.input?.command ?? ''), ws, runApps));
            } else if (c.name === 'Read') {
              if (!String(c.input?.file_path ?? '').startsWith(`${shotsDir}/`)) m.violations.push(`Read ${c.input?.file_path}`);
            } else m.violations.push(`tool ${c.name}`);
          }
          if (c.type === 'text') m.final_message = c.text;
        }
      }
      if (e.type === 'user' && Array.isArray(content)) {
        for (const c of content) {
          if (c.type === 'tool_result' && open.has(c.tool_use_id)) {
            const { t: s, c: use } = open.get(c.tool_use_id);
            toolUse(s, t, c.is_error);
            if (AD) commands.push({ tool: use.name, command: use.input?.command ?? use.input?.file_path, ms: Math.round(t - s), is_error: !!c.is_error });
          }
        }
      }
      if (e.type === 'result') m.usage = { ...e.usage, total_cost_usd: e.total_cost_usd, num_turns: e.num_turns };
    }
  }
  return { metrics: m, commands };
}

async function runOnce(iteration) {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const runDir = join(benchRoot, 'runs', `${stamp}-${task.id}-${o.controller}-${o.agent}-${iteration}`);
  const out = join(runDir, 'out');
  await mkdir(out, { recursive: true });
  if (AD) await mkdir(join(runDir, 'agent'));
  adCwd = out;
  const ws = ad('new', '--label', `linux-cua bench ${task.id}`, '--geometry', GEOMETRY, '--a11y', '--idle', '30');
  let recorder = null;
  let fixture = null;
  const result = { schema: 2, label: o.label, task: task.id, controller: o.controller, agent: o.agent, model: o.model, effort: o.effort ?? null, tier: o.tier ?? null, surface: AD ? null : o.surface, iteration, run_dir: runDir, workspace: ws, started: new Date().toISOString() };
  try {
    const env = workspaceEnv(ws);
    const setupStart = performance.now();
    if (task.setup) fixture = await task.setup({ out, runDir });
    for (const app of task.apps) {
      ad('run', ws, '--', ...app.cmd.map((a) => a.replace('{out}', out)));
      ad('wait', ws, '--class', app.waitClass, '--timeout', '40');
    }
    const mcpEnv = {
      CUA_ENGINE: o.engine, CUA_DISPLAY: env.DISPLAY, XAUTHORITY: env.XAUTHORITY,
      DBUS_SESSION_BUS_ADDRESS: env.DBUS_SESSION_BUS_ADDRESS, CUA_ALLOW_APPS: task.allowApps.join(','),
      ...(env.AT_SPI_BUS_ADDRESS ? { AT_SPI_BUS_ADDRESS: env.AT_SPI_BUS_ADDRESS } : {}),
    };
    if (task.browser) {
      // Brave in both cases: agent-desktop's own CDP-enabled private profile,
      // or for cua the same browser with renderer accessibility and a CDP port.
      if (AD) ad('browser', ws, '--start', '--url', fixture.url);
      else {
        const port = await freePort();
        ad('run', ws, '--', 'brave-browser', '--no-first-run', '--no-default-browser-check', '--password-store=basic', '--force-renderer-accessibility',
          `--user-data-dir=${join(runDir, 'profile')}`, '--remote-debugging-address=127.0.0.1', `--remote-debugging-port=${port}`, '--disable-gpu', fixture.url);
        ad('wait', ws, '--class', 'brave', '--timeout', '40');
        mcpEnv.CUA_BROWSER_CDP = `http://127.0.0.1:${port}`;
      }
      await Promise.race([fixture.firstLoad, new Promise((_, fail) => setTimeout(() => fail(Error('browser did not load the start page')), 40_000))]);
    }
    result.setup_ms = performance.now() - setupStart;
    if (o.record) {
      recorder = spawn('ffmpeg', ['-loglevel', 'error', '-f', 'x11grab', '-video_size', GEOMETRY, '-framerate', '15', '-i', env.DISPLAY,
        '-c:v', 'libx264', '-preset', 'ultrafast', '-crf', '30', '-pix_fmt', 'yuv420p', join(runDir, 'video.mp4')],
      { env: { ...process.env, ...env }, stdio: ['pipe', 'ignore', 'inherit'] });
    }
    const runApps = [...task.apps.map((a) => a.cmd[0]), ...(task.browser ? ['brave-browser'] : [])];
    const shotsDir = join(homedir(), '.cache/agent-desktop/ws', ws);
    let prompt = task.prompt(out, fixture ?? {});
    if (AD) {
      const docs = await skillDocs();
      result.skill_sha256 = docs.sha256;
      prompt += `\n\nThe applications are already open on the private agent-desktop workspace ${ws} (created with --a11y${task.browser ? `; the web browser was started with \`agent-desktop browser ${ws} --start\`` : ''}). Use only agent-desktop commands on workspace ${ws} for every interaction with them.
Rules:
- Every shell command must consist only of \`agent-desktop\` commands on workspace ${ws}. Several may be chained with && or ;, and JSON may be passed as an argument or through a heredoc. No other programs, pipes, command substitution or redirection to files.
- Do not create, close or view workspaces: never run \`agent-desktop new\`, \`close\` or \`view\`. Use \`agent-desktop run ${ws} -- PROGRAM\` only to reopen one of the task's applications (${runApps.join(', ')}).
- Do not read, write, move or create task files with shell tools; do all work through the applications' GUI via agent-desktop.
- ${o.agent === 'claude' ? `You may open screenshot images that agent-desktop writes under ${shotsDir}/ with the Read tool, and nothing else with it.` : `You may view screenshot images that agent-desktop writes under ${shotsDir}/.`}
Any other command or tool use makes the run fail.

The agent-desktop documentation follows.

${docs.text}`;
    } else {
      prompt += '\n\nThe applications are already open on the desktop. Use only the cua computer-use tools for every interaction with them.';
    }
    await writeFile(join(runDir, 'prompt.txt'), prompt);
    const [cmd, args] = agentCommand({ prompt, runDir, mcpEnv, shotsDir });
    await writeFile(join(runDir, 'command.json'), JSON.stringify([cmd, ...args], null, 1));
    const events = [];
    const t0 = performance.now();
    const child = spawn(cmd, args, { cwd: AD ? join(runDir, 'agent') : runDir, stdio: ['ignore', 'pipe', 'pipe'] });
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
    const { metrics, commands } = summarize(events, { ws, runApps, shotsDir });
    Object.assign(result, metrics);
    if (AD) await writeFile(join(runDir, 'commands.json'), JSON.stringify(commands, null, 1));
    if (result.first_tool_ms !== null) result.active_ms = result.last_tool_ms - result.first_tool_ms;
    result.model_ms = result.wall_ms - result.tool_ms;
    result.verify = await task.verify(out, { final_message: result.final_message });
    result.success = result.verify.success && code === 0 && result.violations.length === 0;
  } catch (err) {
    result.error = String(err?.message || err);
    result.success = false;
  } finally {
    if (recorder) {
      recorder.stdin.end('q');
      await new Promise((r) => recorder.on('exit', r));
    }
    try { ad('close', ws); } catch (err) { result.close_error = String(err.message); }
    if (fixture?.close) await fixture.close();
  }
  await mkdir(benchRoot, { recursive: true });
  await appendFile(join(benchRoot, 'results.jsonl'), JSON.stringify(result) + '\n');
  await writeFile(join(runDir, 'result.json'), JSON.stringify(result, null, 2));
  const s = (ms) => (ms == null ? '-' : (ms / 1000).toFixed(1) + 's');
  console.log(`${task.id} ${o.controller} ${o.agent}/${o.model}${o.effort ? '/' + o.effort : ''} #${iteration}: ${result.success ? 'PASS' : 'FAIL'} wall ${s(result.wall_ms)} active ${s(result.active_ms)} tools ${result.tool_calls ?? '-'} (${s(result.tool_ms)}) errors ${result.tool_errors ?? '-'} violations ${result.violations?.length ?? '-'} ${result.error ?? ''} -> ${runDir}`);
  return result;
}

for (let i = 0; i < Number(o.iterations); i++) await runOnce(i);

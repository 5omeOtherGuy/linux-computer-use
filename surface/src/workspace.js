// Private desktops from agent-desktop: CUA_WORKSPACE=new creates one owned by
// this server (closed when the server exits or dies), CUA_WORKSPACE=NAME
// attaches to an existing one. The workspace's environment (DISPLAY,
// XAUTHORITY, its D-Bus and accessibility buses, its process marker) goes to
// the engine and so to every app it launches.
import { execFileSync } from 'node:child_process';
import { accessSync, constants } from 'node:fs';
import path from 'node:path';
import { CuaError } from './errors.js';

// MCP clients may start servers with a reduced PATH (Codex does), so the
// installed location is tried when agent-desktop is not on it.
function agentDesktop(env) {
  if (env.CUA_AGENT_DESKTOP) return env.CUA_AGENT_DESKTOP;
  const dirs = [...String(env.PATH || '').split(':').filter(Boolean), ...(env.HOME ? [path.join(env.HOME, '.local', 'bin')] : [])];
  for (const d of dirs) {
    const p = path.join(d, 'agent-desktop');
    try {
      accessSync(p, constants.X_OK);
      return p;
    } catch {}
  }
  return 'agent-desktop';
}

// `agent-desktop env NAME` prints shell exports and one unset line.
export function parseWorkspaceEnv(text) {
  const set = {};
  const unset = [];
  for (const line of String(text).split('\n')) {
    const m = /^export ([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line.trim());
    if (m) set[m[1]] = m[2].replace(/^'(.*)'$/s, '$1').replace(/'\\''/g, "'").replace(/^"(.*)"$/s, '$1');
    const u = /^unset (.*)$/.exec(line.trim());
    if (u) unset.push(...u[1].split(/\s+/).filter(Boolean));
  }
  return { set, unset };
}

// Returns {env, name, close} with env the caller's environment adjusted to the
// workspace, or null when CUA_WORKSPACE is not set.
export function attachWorkspace(env, log = () => {}) {
  const want = env.CUA_WORKSPACE;
  if (!want) return null;
  const bin = agentDesktop(env);
  // agent-desktop keeps its workspaces under XDG_RUNTIME_DIR.
  const adEnv = { ...env, XDG_RUNTIME_DIR: env.XDG_RUNTIME_DIR || `/run/user/${process.getuid()}` };
  const run = (args) => execFileSync(bin, args, { encoding: 'utf8', timeout: 60_000, env: adEnv, stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  let name = want;
  let owned = false;
  if (want === 'new') {
    const args = ['new', '--a11y', '--label', env.CUA_WORKSPACE_LABEL || 'linux-cua', '--geometry', env.CUA_WORKSPACE_GEOMETRY || '1440x900', '--owner-pid', String(process.pid)];
    try {
      name = run(args).split('\n').filter(Boolean).at(-1);
    } catch (err) {
      throw new CuaError('invalid_config', `agent-desktop could not create a workspace: ${String(err.stderr || err.message).trim().slice(0, 300)}`);
    }
    owned = true;
    log(`workspace ${name} created`);
  }
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(name)) throw new CuaError('invalid_config', `invalid workspace name ${JSON.stringify(name)}`);
  let parsed;
  try {
    parsed = parseWorkspaceEnv(run(['env', name]));
  } catch (err) {
    if (owned) try { run(['close', name]); } catch {}
    throw new CuaError('invalid_config', `no agent-desktop workspace ${name}: ${String(err.stderr || err.message).trim().slice(0, 300)}`);
  }
  if (!parsed.set.DISPLAY || /^:0(\.\d+)?$/.test(parsed.set.DISPLAY)) throw new CuaError('invalid_config', `workspace ${name} has no private display`);
  const next = { ...env };
  for (const k of parsed.unset) delete next[k];
  Object.assign(next, parsed.set, { CUA_DISPLAY: parsed.set.DISPLAY });
  let closed = false;
  const close = () => {
    if (!owned || closed) return;
    closed = true;
    try {
      run(['close', name]);
      log(`workspace ${name} closed`);
    } catch (err) {
      log(`closing workspace ${name} failed: ${String(err.stderr || err.message).trim()}`);
    }
  };
  return { env: next, name, close };
}

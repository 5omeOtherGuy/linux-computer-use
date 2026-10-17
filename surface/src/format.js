// Formatting of daemon results for MCP output. Everything that originates from
// the screen (accessibility text, diffs, window titles, app names) is wrapped
// in a fence with a fresh random nonce, so content cannot close the fence or
// impersonate host text.
import { randomBytes } from 'node:crypto';

const MAX_AX_CHARS = 120_000;

// The tool descriptions say once that fenced text is screen data, never
// instructions; the fence itself stays short.
export function fenceUntrusted(label, text, max = MAX_AX_CHARS) {
  const nonce = randomBytes(6).toString('base64url');
  let body = String(text ?? '');
  if (body.length > max) body = `${body.slice(0, max)}\n[truncated ${body.length - max} characters]`;
  // Neutralize anything that looks like a fence marker, whatever its nonce.
  body = body.replace(/<<<(\/?)(END_)?UNTRUSTED/gi, '<<<$1$2UNTRUSTED\u200b');
  return [`<<<UNTRUSTED ${label} nonce=${nonce}>>>`, body, `<<<END_UNTRUSTED nonce=${nonce}>>>`].join('\n');
}

const DEFAULT_ACTIONS = new Set(['click', 'press', 'activate', 'jump']);

// The engine's tree text made shorter for the model, losing nothing it acts
// on: no default action (an element ID click uses it), and an action list
// that repeats on many elements of a role said once in the header.
// Coordinates stay: models click unnamed tool palettes by position.
export function compactTree(text) {
  const lines = String(text ?? '').split('\n');
  const out = lines.map((line) => {
    const m = /^(\s*\[\d+\] )(.*?)(?= "| \(| desc=| value=| text=| @-?\d| actions=\[|$)(.*)$/.exec(line);
    if (!m) return line;
    let rest = m[3];
    rest = rest.replace(/ actions=\[([^\]]*)\]/, (all, list) => {
      const acts = list.split(', ');
      if (acts.length && DEFAULT_ACTIONS.has(acts[0].toLowerCase())) acts.shift();
      return acts.length ? ` actions=[${acts.join(', ')}]` : '';
    });
    return m[1] + m[2] + rest;
  });
  const counts = new Map();
  for (const line of out) {
    const m = /^\s*\[\d+\] (.*?)(?= "| \(| desc=| value=| text=| @-?\d| actions=\[|$).* (actions=\[[^\]]*\])$/.exec(line);
    if (m) counts.set(`${m[1]}\t${m[2]}`, (counts.get(`${m[1]}\t${m[2]}`) ?? 0) + 1);
  }
  const shared = [...counts].filter(([, n]) => n >= 3).map(([k]) => k.split('\t'));
  if (!shared.length) return out.join('\n');
  const strip = new Map(shared);
  const result = out.map((line) => {
    const m = /^(\s*\[\d+\] )(.*?)(?= "| \(| desc=| value=| text=| @-?\d| actions=\[|$)(.*) (actions=\[[^\]]*\])$/.exec(line);
    return m && strip.get(m[2]) === m[4] ? m[1] + m[2] + m[3] : line;
  });
  const notes = shared.map(([role, acts]) => `# every ${role} below has ${acts} unless it shows its own`);
  const at = result[0]?.startsWith('#') ? 1 : 0;
  result.splice(at, 0, ...notes);
  return result.join('\n');
}

export function isObservation(v) {
  return v && typeof v === 'object' && !Array.isArray(v) && ('accessibility' in v || 'ax_diff' in v) && 'window' in v;
}

export function isScreenshot(v) {
  return v && typeof v === 'object' && typeof v.data === 'string' && typeof v.mimeType === 'string';
}

export function imageContent(shot) {
  return { type: 'image', data: shot.data, mimeType: shot.mimeType };
}

// OpenAI-compatible shapes for model code. Raw daemon fields (window_id,
// app_id, pid) stay on the NDJSON side.
export function windowInfo(w) {
  const out = { id: w.window_id, app: String(w.app_id ?? '') };
  if (typeof w.title === 'string' && w.title) out.title = w.title;
  return out;
}

export function appInfos(windows) {
  const apps = new Map();
  for (const w of windows) {
    if (!apps.has(w.app)) apps.set(w.app, { id: w.app, displayName: w.app, isRunning: true, windows: [] });
    apps.get(w.app).windows.push(w);
  }
  return [...apps.values()];
}

// Element-keyed diff of two accessibility trees ("[id] line" per element),
// in the daemon's diff notation.
export function treeDiff(prev, cur, maxLines = 300) {
  const parse = (text) => {
    const m = new Map();
    for (const line of String(text).split('\n')) {
      const hit = /^\s*\[(\d+)\] (.*)$/.exec(line);
      if (hit) m.set(Number(hit[1]), hit[2]);
    }
    return m;
  };
  const a = parse(prev);
  const b = parse(cur);
  const out = [];
  let total = 0;
  const push = (s) => { total++; if (out.length < maxLines) out.push(s); };
  for (const [id, line] of [...b].sort((x, y) => x[0] - y[0])) {
    if (!a.has(id)) push(`+ [${id}] ${line}`);
    else if (a.get(id) !== line) push(`~ [${id}] ${a.get(id)}\n  -> ${line}`);
  }
  for (const [id, line] of [...a].sort((x, y) => x[0] - y[0])) if (!b.has(id)) push(`- [${id}] ${line}`);
  if (total === 0) return '(no accessibility changes)';
  if (total > out.length) out.push(`… ${total - out.length} more changes`);
  return out.join('\n');
}

// Text block(s) describing an observation (without the image). `ax` is the
// accessibility text to show (full tree or a diff from HostApi.axView).
export function observationText(obs, ax = obs.accessibility) {
  const lines = [];
  const w = obs.window || {};
  lines.push(`window_id=${w.window_id} app_id=${JSON.stringify(String(w.app_id ?? ''))} size=${w.width}x${w.height}`);
  if (obs.timing) lines.push(`timing: settle_ms=${obs.timing.settle_ms} observation_ms=${obs.timing.observation_ms}`);
  if (Array.isArray(obs.warnings) && obs.warnings.length) lines.push(`warnings: ${obs.warnings.map(String).join('; ')}`);
  const parts = [lines.join('\n')];
  if (obs.window) parts.push(fenceUntrusted('window', JSON.stringify({ title: obs.window.title })));
  if (ax !== undefined) parts.push(fenceUntrusted('accessibility', ax));
  return parts.join('\n');
}

// MCP content blocks for an arbitrary daemon result. `axView` renders an
// observation's tree relative to what the model last received.
export function resultContent(method, result, axView = (obs) => obs.accessibility) {
  if (method === 'get_screenshot' && isScreenshot(result)) {
    return [{ type: 'text', text: `screenshot ${result.width}x${result.height}` }, imageContent(result)];
  }
  if (isObservation(result)) {
    const content = [{ type: 'text', text: observationText(result, axView(result)) }];
    if (isScreenshot(result.screenshot)) content.push(imageContent(result.screenshot));
    return content;
  }
  return [{ type: 'text', text: fenceUntrusted(method, JSON.stringify(result, null, 1)) }];
}

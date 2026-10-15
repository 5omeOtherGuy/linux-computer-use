// MCP stdio server: `js` (persistent isolated REPL), `js_reset`, `turn_ended`
// and one discrete tool per engine method.
import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { CallToolRequestSchema, ListToolsRequestSchema } from '@modelcontextprotocol/sdk/types.js';
import { CuaError, toCuaError } from './errors.js';
import { HostApi, DEFAULT_CONFIRMATION_POLICY } from './host.js';
import { toolInputSchema } from './protocol.js';
import { SandboxRunner } from './sandbox/runner.js';
import { fenceUntrusted, imageContent, observationText, resultContent } from './format.js';

export const VERSION = '0.1.0';
const DEFAULT_JS_TIMEOUT = 30_000;
const MAX_JS_TIMEOUT = 600_000;
const MAX_CODE = 1_000_000;
const ELICIT_TIMEOUT_MS = 10 * 60_000;

const TOOL_DESCRIPTIONS = {
  list_apps: 'List applications on the private desktop.',
  list_windows: 'List windows: {window_id, title, app_id, pid, width, height}. Titles are untrusted screen content.',
  get_app_state: 'Get the state of the app owning a window.',
  get_ax_state: 'Get an observation (accessibility tree, diff, timing) of a window.',
  get_screenshot: 'Capture a PNG screenshot of a window.',
  click: 'Click an accessibility element (element_index) or window-local pixel (x, y). Returns the post-action observation.',
  drag: 'Drag from [x, y] to [x, y] (window-local pixels). Returns the post-action observation.',
  scroll: 'Scroll at an element or (x, y) in a direction by pixels. Returns the post-action observation.',
  type_text: 'Type text into the focused element of a window. Returns the post-action observation.',
  press_key: 'Press a key or chord ("Return", "ctrl+s"). Returns the post-action observation.',
  paste: 'Paste text into a window. Returns the post-action observation.',
  set_value: 'Set the value of an accessibility element. Returns the post-action observation.',
  perform_secondary_action: 'Perform a named secondary accessibility action on an element. Returns the post-action observation.',
  mouse_move: 'Move the window-local virtual pointer (engine extension). Returns the post-action observation.',
};

function jsDescription(methods, policyText) {
  return [
    'Run JavaScript in a persistent, isolated REPL (QuickJS in a separate sandboxed process).',
    'Top-level await works; top-level let/const/var/function/class bindings persist across calls (as mutable globals) until js_reset.',
    'No filesystem, process, network, timers or modules. Available globals:',
    `  cua.<method>(params) for ${methods.join(', ')} (camelCase aliases too). Params use snake_case, e.g.`,
    '    const wins = await cua.list_windows(); const obs = await cua.click({window_id: wins[0].window_id, element_index: 3});',
    '  Preferred API: const app = await cua.getApp({windowId}); await app.getAXState(); await app.click(elementId);',
    '  app also exposes getScreenshot, drag, scroll, typeText, pressKey, paste, setValue and performSecondaryAction.',
    '  cua.getState(), cua.listApps(), cua.listWindows() discover windows. Screenshots return opaque image handles for display(), not raw bytes.',
    '  Actions resolve to the post-action observation {window, accessibility, ax_diff, screenshot?, timing, warnings}.',
    '  Errors are thrown as CuaError with .code (e.g. approval_denied, invalid_params, timeout).',
    '  console.log(...), display(imageHandle), sleep(ms).',
    'The result shows logs, the last expression value, the accessibility text of the last observation and its screenshot.',
    'Unawaited calls are cancelled when the cell ends. On timeout or cancellation pending actions are terminated.',
    'Each app needs user approval (asked automatically, once per app per turn).',
    policyText,
  ].join('\n');
}

function strictArgs(args, allowed, tool) {
  if (args === undefined || args === null) return {};
  if (typeof args !== 'object' || Array.isArray(args)) throw new CuaError('invalid_params', `${tool}: arguments must be an object`);
  for (const k of Object.keys(args)) if (!allowed.includes(k)) throw new CuaError('invalid_params', `${tool}: unknown argument "${String(k).slice(0, 64)}"`);
  return args;
}

function errorResult(err) {
  const e = toCuaError(err);
  const text = e.code === 'approval_denied' || e.code === 'approval_required'
    ? `${e.code}: ${e.message}`
    : `${e.code}: ${e.message}${e.data !== undefined ? `\n${fenceUntrusted('error_data', JSON.stringify(e.data))}` : ''}`;
  return { isError: true, content: [{ type: 'text', text }] };
}

export function formatCell(title, r) {
  const head = [];
  head.push(`${title ? `[${title}] ` : ''}${r.ok ? 'ok' : `failed${r.reason ? ` (${r.reason})` : ''}`}`);
  if (r.stateLostBefore) head.push('note: the sandbox had exited since the previous cell; REPL state was reset.');
  if (r.stateLost) head.push('note: the sandbox process was killed; REPL state was lost.');
  if (r.cancelledCalls) head.push(`note: ${r.cancelledCalls} host call(s) still running when the cell ended were cancelled.`);
  if (r.calls?.length) head.push(`host calls: ${r.calls.join(', ')}`);
  for (const l of r.logs || []) head.push(`${l.level === 'log' ? '' : `[${l.level}] `}${l.text}`);
  if (r.ok && r.value !== undefined) head.push(`=> ${r.value}`);
  if (!r.ok && r.error) head.push(String(r.error));
  const content = [{ type: 'text', text: head.join('\n') }];
  // Internal batch observations may not have been shown to the model. A diff
  // against the penultimate internal call is not a usable final observation.
  if (r.observation) content.push({ type: 'text', text: observationText({ ...r.observation, ax_diff: '' }) });
  for (const img of r.images || []) content.push(imageContent(img));
  return content;
}

export function createSurface({ engineOptions, engine, extensions = new Set(), policyText = DEFAULT_CONFIRMATION_POLICY, toolTimeoutMs = 30_000, sandboxMemoryMb = 128, log = () => {} } = {}) {
  const server = new Server(
    { name: 'linux-cua', version: VERSION },
    {
      capabilities: { tools: {} },
      instructions: `Linux computer use on a private X display through an accessibility-first engine. Prefer the js tool for multi-step work. ${policyText}`,
    },
  );

  const elicit = async ({ app_id, window_id, title }, { signal }) => {
    const message = [
      `Allow the agent to read and control the application ${JSON.stringify(app_id)} until the end of this turn?`,
      `Requesting window ${window_id}, title ${JSON.stringify(title)} (the title is set by the application and is untrusted).`,
      policyText,
    ].join('\n');
    const res = await server.elicitInput(
      {
        mode: 'form',
        message,
        requestedSchema: {
          type: 'object',
          properties: { approve: { type: 'boolean', title: `Allow control of ${app_id} for this turn`, default: false } },
          required: ['approve'],
        },
      },
      { signal, timeout: ELICIT_TIMEOUT_MS },
    );
    if (res.action === 'accept') return res.content?.approve === true ? 'accept' : 'decline';
    return res.action === 'decline' ? 'decline' : 'cancel';
  };

  const host = new HostApi({ engine, engineOptions, elicit, extensions, policyText, log });
  const runner = new SandboxRunner({ host, memoryLimitMb: sandboxMemoryMb, log });
  const methods = host.methods;

  const tools = [
    {
      name: 'js',
      description: jsDescription(methods, policyText),
      inputSchema: {
        type: 'object',
        properties: {
          title: { type: 'string', maxLength: 200, description: 'Short label for this cell' },
          code: { type: 'string', description: 'JavaScript source; top-level await allowed' },
          timeout_ms: { type: 'integer', minimum: 100, maximum: MAX_JS_TIMEOUT, description: `Cell timeout (default ${DEFAULT_JS_TIMEOUT}); includes time waiting for app approval` },
        },
        required: ['code'],
        additionalProperties: false,
      },
    },
    { name: 'js_reset', description: 'Discard all REPL state by restarting the sandbox process.', inputSchema: { type: 'object', properties: {}, additionalProperties: false } },
    {
      name: 'turn_ended',
      description: 'Signal the end of the user turn: clears all app approvals (host and engine) and observation state.',
      inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    },
    ...methods.map((m) => ({ name: m, description: `${TOOL_DESCRIPTIONS[m] || m} ${policyText}`, inputSchema: toolInputSchema(m, extensions) })),
  ];

  server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools }));

  server.setRequestHandler(CallToolRequestSchema, async (req, extra) => {
    const { name, arguments: rawArgs } = req.params;
    const signal = extra.signal;
    try {
      if (name === 'js') {
        const args = strictArgs(rawArgs, ['title', 'code', 'timeout_ms'], name);
        if (typeof args.code !== 'string' || args.code.length > MAX_CODE) throw new CuaError('invalid_params', 'js: code must be a string up to 1 MB');
        if (args.title !== undefined && (typeof args.title !== 'string' || args.title.length > 200)) throw new CuaError('invalid_params', 'js: title must be a short string');
        const timeout = args.timeout_ms ?? DEFAULT_JS_TIMEOUT;
        if (!Number.isInteger(timeout) || timeout < 100 || timeout > MAX_JS_TIMEOUT) throw new CuaError('invalid_params', `js: timeout_ms must be an integer in [100, ${MAX_JS_TIMEOUT}]`);
        const r = await runner.eval({ code: args.code, timeoutMs: timeout, signal });
        return { isError: !r.ok, content: formatCell(args.title, r) };
      }
      if (name === 'js_reset') {
        strictArgs(rawArgs, [], name);
        runner.reset();
        return { content: [{ type: 'text', text: 'REPL reset: all bindings discarded.' }] };
      }
      if (name === 'turn_ended') {
        strictArgs(rawArgs, [], name);
        runner.cancelActive();
        const r = await host.turnEnded({ signal });
        return { content: [{ type: 'text', text: `Turn ended: app approvals cleared (engine ${r.engine}).` }] };
      }
      if (methods.includes(name)) {
        const result = await host.call(name, rawArgs ?? {}, { signal, deadline: Date.now() + toolTimeoutMs });
        return { content: resultContent(name, result) };
      }
      throw new CuaError('unknown_tool', `unknown tool: ${String(name).slice(0, 64)}`);
    } catch (err) {
      return errorResult(err);
    }
  });

  const close = async () => {
    await runner.close();
    await host.close();
  };
  return { server, host, runner, close };
}

# linux-cua surface

MCP stdio server and provider adapters for the `linux-cua` engine (see `../CONTRACT.md`).

```
MCP client ──stdio──▶ bin/linux-cua-mcp.js (trusted host)
                        ├─ HostApi: validation, per-app approval (MCP elicitation), turn state, action lock
                        │    └─ EngineClient ──NDJSON──▶ Rust daemon (CUA_ENGINE, CUA_DISPLAY)
                        └─ SandboxRunner ──IPC──▶ sandbox process (node --permission, empty env)
                                                     └─ QuickJS/WASM VM: model code, `cua` API only
```

## Running

```sh
cd surface && npm ci
CUA_DISPLAY=:99 \
CUA_ENGINE=/data/scratch/linux-cua-build/target/debug/linux-cua \
node bin/linux-cua-mcp.js
```

MCP client configuration (stdio):

```json
{ "command": "node", "args": ["/path/to/surface/bin/linux-cua-mcp.js"],
  "env": { "CUA_DISPLAY": ":99", "CUA_ENGINE": "/data/scratch/linux-cua-build/target/debug/linux-cua" } }
```

| Variable | Meaning |
|---|---|
| `CUA_DISPLAY` | Required. Explicit X display (e.g. `:99`). `:0` is refused; the server exits with status 2. Passed to the daemon. |
| `CUA_ENGINE` | Daemon executable. Default `../target/debug/linux-cua` relative to `surface/`. With `CARGO_TARGET_DIR=/data/scratch/linux-cua-build/target`, point it there. |
| `CUA_ALLOW_APPS`, `CUA_DENY_APPS` | Inherited by the daemon, which enforces them. |
| `CUA_APPROVALS_FILE` | "Always allow" decisions, `{"always_allowed_app_ids": [...]}`. Default `$XDG_CONFIG_HOME/linux-cua/approvals.json` (or `~/.config/...`). Written only by this host after the user picks "Always allow"; the operator may edit or delete it. Without a location, only turn and session approval are offered. |
| `CUA_ENGINE_EXTENSIONS` | Comma list of optional engine features beyond CONTRACT.md: `click_button`, `click_count`, `modifiers`, `mouse_move`. Off by default. Enable only if the daemon implements them. |
| `CUA_CONFIRMATION_POLICY` | Replaces the default confirmation-policy text shown in tool descriptions, server instructions and approval prompts. |
| `CUA_TOOL_TIMEOUT_MS` | Timeout for discrete tools (default 30000). |
| `CUA_SANDBOX_MEMORY_MB` | QuickJS heap limit (default 128). |

The daemon's stderr is forwarded to the server's stderr. stdout carries only MCP. The sandbox process (about 250 ms to fork Node and initialise QuickJS) and the daemon start when the server starts, so the first `js` cell does not pay for them.

## Tools

- `js {title?, code, timeout_ms?}` runs a cell in the persistent REPL. Top-level `await` works. Top-level `let/const/var/function/class` bindings persist across cells as mutable globals, and redeclaring them is allowed. The result contains, in order, everything the cell emitted (auto-emitted observations, `nodeRepl.write`, `nodeRepl.emitImage`, `console.log`), the value of the last expression (also inside a trailing block, `if` or `try`), and, if the cell acted after its last emitted observation of that window, the state after the last action (observed once, when the cell ends: the actions themselves run with `observe: false`). `timeout_ms` defaults to 30000 (maximum 600000). Time spent waiting for app approval counts toward it.
- `js_reset` discards all REPL state by restarting the sandbox process.
- `turn_ended` cancels an active JS cell and old-turn queued requests, clears this turn's approvals and denials (session and always approvals stay), and resets the daemon. If the daemon has a pending request or cannot confirm the reset, it is killed. Idle REPL bindings survive.
- Discrete tools, one per engine method: `list_apps`, `list_windows`, `get_app_state`, `get_ax_state`, `get_screenshot`, `click`, `drag`, `scroll`, `type_text`, `press_key`, `paste`, `set_value`, `perform_secondary_action`, plus `mouse_move` with that extension. Their JSON Schemas come from the same table the host uses for validation (`src/protocol.js`).

### JS API (inside the VM)

The names and shapes follow OpenAI's computer-use JavaScript API, which GPT models are trained on.

```js
const windows = await cua.listWindows();                  // WindowInfo[] {id, app, title?}; emitted
const apps = await cua.listApps({emit: false});           // AppInfo[] {id, displayName, isRunning, windows}
const { apps: running } = await cua.getState();           // {apps, browsers: []}
const app = await cua.getApp({windowId: windows[0].id});  // or {appId}; App {id, displayName, isRunning, windows, window, windowId, ...methods}
await app.getAXState();                     // full tree first, then changes since the tree you last received; emitted
await app.getAXState({disableDiffing: true, emit: false});
await app.setValue(observedFieldId, 'Hello');
await app.click(observedButtonId);          // or app.click([x, y]); resolves when the UI settled
const png = await app.getScreenshot();      // Uint8Array PNG bytes; emitted
const { state, screenshot } = await app.getAXStateAndScreenshot({emit: false});
nodeRepl.write({ any: 'value' });           // text output (strings as-is, other values as JSON)
await nodeRepl.emitImage(png);              // also {bytes, mimeType} or a data:image/png;base64 URL
try { await app.click(13) } catch (e) { e.code /* approval_denied, invalid_params, timeout, ... */ }
console.log(...); display(png); await sleep(250);   // aliases and helpers
```

Observation and discovery calls (`getAXState`, `getScreenshot`, `getAXStateAndScreenshot`, `cua.getState`, `getApp`, `listApps`, `listWindows`) add their result to the cell output; pass `{emit: false}` to suppress it. Accessibility diffs are always relative to the last tree the model actually received for that window (the last emitted one, including the post-action state shown at the end of a cell), never to an internal observation inside a cell. Actions resolve to `undefined`; their warnings are emitted. The VM has no `require`, `process`, `fetch`, `import()`, `WebAssembly`, timers or filesystem. `cua`, `nodeRepl` and the helpers are frozen, non-writable globals. Raw `window_id` methods are not exposed to model code.

## Security model

- Isolation. Model code runs in QuickJS compiled to WebAssembly (`quickjs-emscripten`), not in Node and not in Node's `vm` module, which is not a security boundary. The only capabilities are three native functions: host call, output and sleep. The prelude captures them and then deletes them from the global object. As a second layer, the VM runs in a separate Node process started with `--permission`, read access only to `surface/`, no write, child process, worker, addon or WASI permission, `--disallow-code-generation-from-strings`, a 256 MB V8 heap and an empty environment. The process holds no credentials and no daemon handle. It only talks to the trusted host over IPC.
- Revalidation. Every `cua.*` call, discrete tool call and adapter call goes through `HostApi.call`, which checks it against the method table. The checks cover the method name, exact parameter keys, integer ranges, enums and string limits. The host then builds a fresh params object. `approve_app` and `turn_ended` cannot be requested by any caller. Unknown arguments to MCP tools are rejected.
- Authorization. The daemon decides when approval is needed and answers `approval_required {app_id, window_id, title}`. The host then sends an MCP form elicitation with a `decision` field: allow for this turn, allow for this session, always allow (only when an approvals file location exists), or deny. Only an explicit allow leads to `approve_app {window_id, app_id}`. Session and always approvals are re-granted to the daemon after `turn_ended` without asking again; "always" is written to `CUA_APPROVALS_FILE`. A decline is remembered until `turn_ended`, while a dismissal is not remembered. Daemon deny lists and hard denies win over every approval. A client without elicitation support gets `approval_unavailable`, so approval fails closed. Approval prompts show the window title sanitized and labelled as untrusted. Deny lists stay with the daemon.
- Cancellation and timeouts. Engine requests are serialized. Cancellation kills the issuing daemon to stop further dispatch, but cannot recall events already delivered to an app: outcomes may be partial or unknown. The next request restarts the daemon with current-turn approvals; it does not replay the interrupted action. Unawaited calls are cancelled at cell end. Calls from finished cells are rejected. QuickJS interrupts overdue cells, with a hard process-kill backstop that reports lost REPL state.
- Untrusted content. Accessibility text, diffs, window titles and other daemon data are wrapped in `<<<UNTRUSTED label nonce=…>>> … <<<END_UNTRUSTED nonce=…>>>`. Each fence gets a fresh random nonce, and fence-like sequences inside the content are neutralized.
- Confirmation policy. The policy text is surfaced in the server instructions, the tool descriptions and the approval prompts. It is advisory: the server does not try to classify actions as consequential.

## Provider adapters

Both adapters drive one window: the provider's "screen" is that window. Both take any `host` with `call(method, params, {signal})`, normally a `HostApi`, so validation and approval are the same as for MCP tools.

### Anthropic `computer_toolset_20260801`

Source: platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool, read 2026-10-15.

```js
import { HostApi, AnthropicComputerToolset } from 'linux-cua-surface';
const toolset = new AnthropicComputerToolset({ host, windowId: 11 });
tools: [toolset.toolDefinition(), ...]           // {type: 'computer_toolset_20260801', configs: {right_click: {enabled: false}, ...}}
const results = await toolset.runBatch(message.content);   // tool_result blocks, each with toolset_name: 'computer'
```

- Members the engine cannot perform are declared `enabled: false`. Without extensions these are `right_click`, `middle_click`, `double_click`, `triple_click`, `mouse_move`, `left_mouse_down`, `left_mouse_up` and `hold_key`. If Claude calls one of them anyway, it gets `is_error`.
- Batches run in order and stop at the first failure. Every later block gets exactly `Not executed: an earlier computer action in this turn failed.` Duplicate ids, a wrong `toolset_name`, unknown members, unknown input fields (for example a leftover `action`), out-of-bounds coordinates and invalid values are rejected before that block acts. An optional `confirm(block)` hook runs before each block. When a batch succeeds and does not end with `screenshot` or `zoom`, a screenshot is attached to the last result (`attachFinalScreenshot`).
- Screenshots are resized to the toolset models' limits: at most 2576 px on the long edge and at most 4784 visual tokens, counted as `ceil(w/28)·ceil(h/28)`. The legacy limits are available as `ANTHROPIC_LEGACY_LIMITS`. Coordinates are mapped from the latest screenshot's pixel space to window pixels by pixel centre. `zoom` crops the window-space region at full resolution and fits it into the screenshot size, keeping the aspect ratio. `key.repeat` (1–100) presses the key that many times. `scroll_amount` is multiplied by `scrollPixelsPerClick` (default 100). `cursor_position` reports the adapter's last pointer position as `[x, y]` in screenshot space. Before any pointer action it fails.

### OpenAI Responses `computer` tool

Sources: developers.openai.com/api/docs/guides/tools-computer-use and …/tools-computer-use-integration, and the public API reference for `ComputerAction`. Read 2026-10-15. Only public documentation was used.

```js
import { OpenAIComputerTool } from 'linux-cua-surface';
const computer = new OpenAIComputerTool({ host, windowId: 11, limits: { maxWidth: 1600, maxHeight: 900 }, confirmSafetyChecks });
tools: [computer.toolDefinition()]                // {type: 'computer'}
const { output, error, refused } = await computer.handleCall(computerCallItem);
// output: {type: 'computer_call_output', call_id, acknowledged_safety_checks, output: {type: 'computer_screenshot', image_url: 'data:image/png;base64,…', detail: 'original'}}
```

- The adapter accepts `actions` (batched) or the older single `action`. The whole batch is validated and translated before anything runs. One invalid action means none run, and the current screenshot is still returned with `error`.
- Mappings:

  | Action | Engine call |
  |---|---|
  | `click` | `click` (`left`; `right` and `wheel` need `click_button`; `back` and `forward` are unsupported) |
  | `double_click` | `click_count: 2` (needs `click_count`) |
  | `drag` | `drag` from the first to the last path point. Paths with more than two points are rejected unless `allowDragPathSimplification` is set. |
  | `move` | needs `mouse_move` |
  | `scroll` | one `scroll` per non-zero axis, with direction from the sign and distance scaled to window pixels |
  | `keypress` | keys normalized to X keysyms (`ENTER`→`Return`, `CTRL`→`ctrl`, `ARROWLEFT`→`Left`, `A`→`a`) and joined with `+` |
  | `type` | `type_text` |
  | `wait` | `waitMs` (default 1000) |
  | `screenshot` | no-op, because the output screenshot is captured after the batch |

- Mouse `keys` (held modifiers) need the `modifiers` extension.
- When `pending_safety_checks` is non-empty, nothing runs unless `confirmSafetyChecks(checks, call)` returns true. The checks are then echoed in `acknowledged_safety_checks`.
- Screenshots use `detail: "original"`. They are downscaled only when `limits` (`maxWidth`, `maxHeight`, `maxLongEdge`, `maxPixels`) require it, and coordinates are then mapped back.

## Integration notes for the engine (parent-owned)

The surface relies on these daemon behaviours:

1. Errors are `{code: string, message, data?}`. The host interprets only `approval_required`, which must have `data: {app_id, window_id, title}`. The host retries the original request after `approve_app` succeeds, at most twice. Any other code is passed through to the caller as given.
2. `list_windows` returns an array of `{window_id, title, app_id, pid, width, height}`. The adapters use width and height until the first screenshot. `get_screenshot` returns `{data, mimeType: 'image/png', width, height}`. Actions and `get_ax_state` return the observation object; an action sent with `observe: false` (every action inside a `js` cell, and every adapter action) returns it without `accessibility` and with `observed: false`, and the surface observes with `get_ax_state` when the cell ends. The surface treats `screenshot` in an observation as optional.
3. The daemon may be killed with SIGKILL at any time, after a timeout, a cancellation or a protocol violation, and is respawned with the same environment. Approvals granted during the turn are replayed with `approve_app`. Replay failures are ignored, and the next request then triggers `approval_required` again.
4. Any non-JSON line on stdout kills the daemon. Responses with unknown ids are dropped.
5. The surface never sends parameters outside CONTRACT.md unless `CUA_ENGINE_EXTENSIONS` enables them. If the parent adds `button`, `click_count`, `modifiers` or a `mouse_move` method, the daemon must implement them, or reject unsupported values with an error rather than ignore them.
6. `turn_ended` is sent only by the host. A daemon error on `turn_ended` causes a kill and respawn, which leaves no approvals.

Smoke check against the real daemon (no GUI is launched by the surface): start the server with `CUA_DISPLAY` set to the private display, then call `list_windows` and `js` with `await (await cua.getApp({windowId})).getAXState()` from any MCP client that supports form elicitation.

## Tests

```sh
npm test
```

The tests use `test/fake-engine.js`, an NDJSON test double with daemon-side approval, a deny list, a slow action and a log of completed actions. They cover:

- REPL persistence, completion values and `js_reset`.
- The OpenAI-compatible shapes, `nodeRepl` output, auto-emit and `{emit: false}`, PNG bytes, and diffs relative to the last emitted tree.
- Approval once per app per turn, session and always scopes (the approvals file, deny lists winning), denial memory and reset on `turn_ended`, and failing closed without elicitation.
- The daemon deny list.
- Injection boundaries: host-only methods, Node globals, `import()`, strict parameters, `__proto__`, fence escapes in AX text and titles, and calls from finished cells.
- Timeout and MCP cancellation, proving via the engine log that the pending action never completed. Approval replay after a restart.
- Sandbox memory limit, the hard kill of an unresponsive sandbox, output caps, and the sandbox process flags and empty environment.
- Both provider adapters: definitions, coordinate transforms, image limits, batch halting and validation, zoom, extensions, safety checks and key normalization.

## Limitations

- Node 22's permission model does not restrict network sockets, so the sandbox process could open sockets if the VM were escaped. The QuickJS VM itself has no network API. For defence in depth against a QuickJS or WASM escape, run the server in a network namespace or container.
- QuickJS checks for interrupts periodically. Loops dominated by native work (for example huge `String.repeat`) may run until the parent hard-kills the sandbox, and then REPL state is lost and reported. The memory limit counts QuickJS allocations only.
- Top-level `const` in the REPL becomes a mutable global. Top-level functions are hoisted per cell and published to `globalThis`. ES module syntax (`import`/`export`) is not supported.
- The contract has no hover, mouse button down/up, key hold, cursor query, or modifier/button/multi-click parameters. The adapters disable or reject these instead of emulating them. `cursor_position` reports the adapter's own record of the last pointer position, not a value queried from the engine.
- Adapters treat one window as the screen. Each coordinate is mapped against the most recent screenshot, so if the window is resized between screenshots, coordinates follow the latest one.
- Elicitation waiting time counts toward the `js` cell timeout and the discrete tool timeout.
- Unit/contract tests use the fake daemon; `../bench/integration.mjs` exercises the real Rust engine and GTK fixture on an explicitly owned private display. Neither adapter has been tested with live model inference yet.

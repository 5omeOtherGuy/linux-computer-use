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
CUA_ENGINE=/path/to/target/debug/linux-cua \
node bin/linux-cua-mcp.js
```

MCP client configuration (stdio):

```json
{ "command": "node", "args": ["/path/to/surface/bin/linux-cua-mcp.js"],
  "env": { "CUA_DISPLAY": ":99", "CUA_ENGINE": "/path/to/target/debug/linux-cua" } }
```

| Variable | Meaning |
|---|---|
| `CUA_DISPLAY` | Required. Explicit X display (e.g. `:99`). `:0` is refused; the server exits with status 2. Passed to the daemon. |
| `CUA_ENGINE` | Daemon executable. Default `../target/debug/linux-cua` relative to `surface/`. With a custom `CARGO_TARGET_DIR`, point it there. |
| `CUA_ALLOW_APPS`, `CUA_DENY_APPS` | Inherited by the daemon, which enforces them. |
| `CUA_APPROVALS_FILE` | "Always allow" decisions, `{"always_allowed": [{"app_id": "Xed", "executable": "/usr/bin/xed"}]}`: an entry matches only that app ID together with the executable the daemon reports for the window's process. Default `$XDG_CONFIG_HOME/linux-cua/approvals.json` (or `~/.config/...`). Read once at startup; written only by this host after the user picks "Always allow"; the operator may edit or delete it. A file that cannot be read or parsed, including the old `{"always_allowed_app_ids": [...]}` format, is reported on stderr, treated as empty and never overwritten, and "always" is not offered until it is fixed. Without a location, only turn and session approval are offered. |
| `CUA_ENGINE_EXTENSIONS` | Accepted for compatibility (`click_button`, `click_count`, `modifiers`, `mouse_move`); these are part of the contract now and always available. |
| `CUA_CONFIRMATION_POLICY` | Replaces the default confirmation-policy text shown in tool descriptions, server instructions and approval prompts. |
| `CUA_TOOL_TIMEOUT_MS` | Timeout for discrete tools (default 30000). |
| `CUA_SANDBOX_MEMORY_MB` | QuickJS heap limit (default 128). |
| `CUA_BROWSER_CDP` | Debugging endpoint of a Chromium-family browser for `cua.getBrowser()`, e.g. `http://127.0.0.1:9333` (start it with `--remote-debugging-address=127.0.0.1 --remote-debugging-port=9333 --password-store=basic` and a disposable `--user-data-dir`). Only loopback endpoints are accepted. Unset: browser calls fail with `browser_unavailable`. |
| `CUA_BROWSER_ALLOW_FILE` | `1` lets `tab.goto` open `file:` URLs, which show any file the browser's user can read. Unset: only http and https URLs. |

The daemon's stderr is forwarded to the server's stderr. stdout carries only MCP. The sandbox process (about 250 ms to fork Node and initialise QuickJS) and the daemon start when the server starts, so the first `js` cell does not pay for them.

## Tools

- `js {title?, code, timeout_ms?}` runs a cell in the persistent REPL. Top-level `await` works. Top-level `let/const/var/function/class` bindings persist across cells as mutable globals, and redeclaring them is allowed. The result contains, in order, everything the cell emitted (auto-emitted observations, `nodeRepl.write`, `nodeRepl.emitImage`, `console.log`), the value of the last expression (also inside a trailing block, `if` or `try`), and, if the cell acted after its last emitted observation of that window, the state after the last action (observed once, when the cell ends: the actions themselves run with `observe: false`). `timeout_ms` defaults to 30000 (maximum 600000). Time spent waiting for app approval counts toward it.
- `js_reset` discards all REPL state by restarting the sandbox process.
- `turn_ended` cancels an active JS cell and old-turn queued requests, clears this turn's approvals and denials (session and always approvals stay), and resets the daemon. If the daemon has a pending request or cannot confirm the reset, it is killed. Idle REPL bindings survive.
- Discrete tools, one per engine method: `list_apps`, `list_windows`, `get_app_state`, `get_ax_state`, `get_screenshot`, `launch_app`, `clipboard_write`, `clipboard_read`, `clipboard_release`, `click`, `drag`, `scroll`, `mouse_move`, `type_text`, `press_key`, `paste`, `set_value`, `perform_secondary_action`. Their JSON Schemas come from the same table the host uses for validation (`src/protocol.js`); `click` takes `button`, `click_count`, `modifiers` and `hold_ms`, `drag` and `scroll` take `modifiers`, `press_key` takes `hold_ms`, and observations and actions take `screenshot: true` to return the window's image in the same call.

### JS API (inside the VM)

The names and shapes follow OpenAI's computer-use JavaScript API, which GPT models are trained on.

```js
const windows = await cua.listWindows();                  // WindowInfo[] {id, app, title?}; emitted
const apps = await cua.listApps({emit: false});           // AppInfo[] {id, displayName, isRunning, windows}
const { apps: running } = await cua.getState();           // {apps, browsers: []}
const app = await cua.getApp({windowId: windows[0].id});  // or {appId}; App {id, displayName, isRunning, windows, window, windowId, ...methods}
const started = await cua.launchApp('drawing');           // starts an installed app (desktop ID, menu name or program); App with .pid
await app.getAXState();                     // full tree first, then changes since the tree you last received; emitted
await app.getAXState({disableDiffing: true, emit: false});
await app.setValue(observedFieldId, 'Hello');
await app.click(observedButtonId);          // or app.click([x, y]); resolves when the UI settled
await app.click(id, {modifiers: 'shift', durationMs: 300}); // held modifiers (shift, ctrl, alt, super) and press duration
await app.rightClick(id); await app.doubleClick(id); await app.moveMouse([x, y]); await app.moveMouseRelative(10, 0);
await app.drag([x, y], [x2, y2], {modifiers: 'ctrl'}); await app.scroll([x, y], 'down', 1, {modifiers: 'shift'});
await app.pressKey('Down', {durationMs: 500});
const png = await app.getScreenshot();      // Uint8Array PNG bytes; emitted
const { state, screenshot } = await app.getAXStateAndScreenshot({emit: false}); // one engine request
await cua.clipboard.write('text'); await cua.clipboard.read(); await cua.clipboard.release();
cua.help();                                 // prints the js tool description again
nodeRepl.write({ any: 'value' });           // text output (strings as-is, other values as JSON)
await nodeRepl.emitImage(png);              // also {bytes, mimeType} or a data:image/png;base64 URL
try { await app.click(13) } catch (e) { e.code /* approval_denied, invalid_params, timeout, ... */ }
console.log(...); display(png); await sleep(250);   // aliases and helpers
```

Observation and discovery calls (`getAXState`, `getScreenshot`, `getAXStateAndScreenshot`, `cua.getState`, `getApp`, `listApps`, `listWindows`) add their result to the cell output; pass `{emit: false}` to suppress it. Accessibility diffs are always relative to the last tree the model actually received for that window (the last emitted one, including the post-action state shown at the end of a cell), never to an internal observation inside a cell. Actions resolve to `undefined`; their warnings are emitted. The VM has no `require`, `process`, `fetch`, `import()`, `WebAssembly`, timers or filesystem. `cua`, `nodeRepl` and the helpers are frozen, non-writable globals. Raw `window_id` methods are not exposed to model code.

### Browser tab (CDP)

`cua.getBrowser()` (alias `cua.getTab()`) drives the first page tab of the browser at `CUA_BROWSER_CDP` over the DevTools protocol (`src/browser/`), porting agent-desktop's browser controller. The tab has the App methods that make sense for a page, with the same argument shapes and locators:

```js
const tab = await cua.getBrowser();          // emits the page state, like getApp
await tab.goto('https://example.org/');       // http(s) only (file: with CUA_BROWSER_ALLOW_FILE=1); resolves when the document loaded
await tab.setValue({role: 'textbox', name: 'Search'}, 'query'); // focus, select all, insert, exact readback (else value_mismatch); <select>: option by label
await tab.click({role: 'button', name: 'Go'});  // or click(id) / click([x, y]); scrolls into view, refuses covered or hidden elements
await tab.pressKey('Return'); await tab.typeText('more');
await tab.waitFor({url: '/results'});         // substring or RegExp; also {text}, {role, name}; timeoutMs (default 10000)
await tab.read(id, {row: true});              // {tag, text, href?, links: [{text, url}]} of the element or its list item/table row
await tab.find({role: 'link', text: /issue/}); await tab.getAXState({query: 'Save'}); await tab.getScreenshot();
tab.url; tab.title;                           // as of the last call
```

Observations come from `Accessibility.getFullAXTree` plus one `DOMSnapshot` for boxes, rendered in the native line format: IDs are the nodes' `backendDOMNodeId` (stable for a document), coordinates are viewport CSS pixels (omitted on named widgets; `(offscreen)` below the fold), ignored nodes and unnamed wrappers are dissolved, text that repeats a name is dropped and text-only containers show `text="…"`. A state is capped at 16,000 characters with a note pointing to `getAXState({query})` and `find()`. After a cell that acted on the tab, its state is emitted once at the end. Input waits 60 ms and, if a navigation started, for its DOMContentLoaded (at most 10 s).

## Security model

- Isolation. Model code runs in QuickJS compiled to WebAssembly (`quickjs-emscripten`), not in Node and not in Node's `vm` module, which is not a security boundary. The only capabilities are three native functions: host call, output and sleep. The prelude captures them and then deletes them from the global object. As a second layer, the VM runs in a separate Node process started with `--permission`, read access only to `surface/`, no write, child process, worker, addon or WASI permission, `--disallow-code-generation-from-strings`, a 256 MB V8 heap and an empty environment. The process holds no credentials and no daemon handle. It only talks to the trusted host over IPC.
- Revalidation. Every `cua.*` call, discrete tool call and adapter call goes through `HostApi.call`, which checks it against the method table. The checks cover the method name, exact parameter keys, integer ranges, enums and string limits. The host then builds a fresh params object. `approve_app` and `turn_ended` cannot be requested by any caller. Unknown arguments to MCP tools are rejected.
- Authorization. The daemon decides when approval is needed and answers `approval_required {app_id, window_id, title, executable}` (also for `clipboard_read` of text an app owns, naming that app's window). The host then sends an MCP form elicitation with a `decision` field: allow for this turn, allow for this session, always allow (only when an approvals file location exists), or deny. Only an explicit allow leads to `approve_app {window_id, app_id}`. Session and always approvals are re-granted to the daemon after `turn_ended` without asking again; "always" is written to `CUA_APPROVALS_FILE`. A decline is remembered until `turn_ended`, while a dismissal is not remembered. Daemon deny lists and hard denies win over every approval. A client without elicitation support gets `approval_unavailable`, so approval fails closed. Approval prompts show the window title sanitized and labelled as untrusted. Deny lists stay with the daemon.
- Cancellation and timeouts. Engine requests are serialized. Cancellation kills the issuing daemon to stop further dispatch, but cannot recall events already delivered to an app: outcomes may be partial or unknown. The next request restarts the daemon with current-turn approvals; it does not replay the interrupted action. Unawaited calls are cancelled at cell end. Calls from finished cells are rejected. QuickJS interrupts overdue cells, with a hard process-kill backstop that reports lost REPL state.
- Untrusted content. Accessibility text, diffs, window titles and other daemon data are wrapped in `<<<UNTRUSTED label nonce=…>>> … <<<END_UNTRUSTED nonce=…>>>`. Each fence gets a fresh random nonce, and fence-like sequences inside the content are neutralized.
- Browser. Only the operator's loopback `CUA_BROWSER_CDP` endpoint is used, and the page socket must be on that host and port. Model code gets no page JavaScript: the host runs only fixed functions (click point and occlusion check, edit target, readback, read) in an isolated world, with model strings as arguments. Navigation accepts http and https URLs, and file URLs only when the operator sets `CUA_BROWSER_ALLOW_FILE=1`. Page text, URLs and titles reach the model inside untrusted fences. The browser needs no per-app approval: configuring the endpoint is the operator's grant, so point it at a disposable profile, never one with the owner's logins.
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

- Members the engine cannot perform are declared `enabled: false`: `left_mouse_down` and `left_mouse_up` (the engine presses and releases within one action). If Claude calls one of them anyway, it gets `is_error`. `hold_key` maps to `press_key` with `hold_ms` (duration up to 10 s); modifier `text` on clicks, drags and scrolls maps to `modifiers`; `mouse_move` maps to `mouse_move`.
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
  | `click` | `click` (`left`, `right`, `wheel` as middle; `back` and `forward` are unsupported) |
  | `double_click` | `click_count: 2` |
  | `drag` | `drag` from the first to the last point, or one continuous stroke through all of them (`path`, up to 256 points) |
  | `move` | `mouse_move` (held keys are not supported on a move) |
  | `scroll` | one `scroll` per non-zero axis, with direction from the sign and distance scaled to window pixels |
  | `keypress` | keys normalized to X keysyms (`ENTER`→`Return`, `CTRL`→`ctrl`, `ARROWLEFT`→`Left`, `A`→`a`) and joined with `+` |
  | `type` | `type_text` |
  | `wait` | `waitMs` (default 1000) |
  | `screenshot` | no-op, because the output screenshot is captured after the batch |

- Mouse `keys` (held modifiers) map to `modifiers`.
- When `pending_safety_checks` is non-empty, nothing runs unless `confirmSafetyChecks(checks, call)` returns true. The checks are then echoed in `acknowledged_safety_checks`.
- Screenshots use `detail: "original"`. They are downscaled only when `limits` (`maxWidth`, `maxHeight`, `maxLongEdge`, `maxPixels`) require it, and coordinates are then mapped back.

## Integration notes for the engine (parent-owned)

The surface relies on these daemon behaviours:

1. Errors are `{code: string, message, data?}`. The host interprets only `approval_required`, which must have `data: {app_id, window_id, title}`. The host retries the original request after `approve_app` succeeds, at most twice. Any other code is passed through to the caller as given.
2. `list_windows` returns an array of `{window_id, title, app_id, pid, width, height}`. The adapters use width and height until the first screenshot. `get_screenshot` returns `{data, mimeType: 'image/png', width, height}`. Actions and `get_ax_state` return the observation object; an action sent with `observe: false` (every action inside a `js` cell, and every adapter action) returns it without `accessibility` and with `observed: false`, and the surface observes with `get_ax_state` when the cell ends. The surface treats `screenshot` in an observation as optional.
3. The daemon may be killed with SIGKILL at any time, after a timeout, a cancellation or a protocol violation, and is respawned with the same environment. Approvals granted during the turn are replayed with `approve_app`. Replay failures are ignored, and the next request then triggers `approval_required` again.
4. Any non-JSON line on stdout kills the daemon. Responses with unknown ids are dropped.
5. The surface never sends parameters outside CONTRACT.md. The daemon rejects unsupported values with an error rather than ignoring them.
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
- Held modifiers, press durations, hover and relative moves, mouse buttons, the one-request tree-plus-screenshot, `launchApp`, the clipboard and `cua.help()`.
- The browser tab: AX rendering and pruning against a recorded tree (`test/fixtures/browser-ax.json`, from `test/fixtures/browser-page.js`), locator resolution in the sandbox, key, URL and endpoint validation; with `CUA_BROWSER_CDP` set, a real browser fills, submits and reads that page.
- Both provider adapters: definitions, coordinate transforms, image limits, batch halting and validation, zoom, modifiers and click variants, held keys, moves, safety checks and key normalization.

## Limitations

- Node 22's permission model does not restrict network sockets, so the sandbox process could open sockets if the VM were escaped. The QuickJS VM itself has no network API. For defence in depth against a QuickJS or WASM escape, run the server in a network namespace or container.
- QuickJS checks for interrupts periodically. Loops dominated by native work (for example huge `String.repeat`) may run until the parent hard-kills the sandbox, and then REPL state is lost and reported. The memory limit counts QuickJS allocations only.
- Top-level `const` in the REPL becomes a mutable global. Top-level functions are hoisted per cell and published to `globalThis`. ES module syntax (`import`/`export`) is not supported.
- The contract has no separate mouse button down/up across calls and no cursor query. The adapters disable or reject these instead of emulating them. `cursor_position` reports the adapter's own record of the last pointer position, not a value queried from the engine.
- Adapters treat one window as the screen. Each coordinate is mapped against the most recent screenshot, so if the window is resized between screenshots, coordinates follow the latest one.
- Elicitation waiting time counts toward the `js` cell timeout and the discrete tool timeout.
- Unit/contract tests use the fake daemon; `../bench/integration.mjs` exercises the real Rust engine and GTK fixture on an explicitly owned private display. Neither adapter has been tested with live model inference yet.

### Continuous canvas strokes

`await app.drag({path: [[100,100], [180,100], [180,180], [100,100]]})` sends every segment with the left button held, then releases and settles once. Paths contain 2–256 integer window-local points; `{x,y}` points also work in the JS API. Use a pencil/brush for arbitrary outlines, and inspect a screenshot after a group of strokes. `drag(from, to)` is unchanged. Discrete `drag` accepts `path` instead of `from`/`to`; the OpenAI adapter sends a multi-point `drag` path as one continuous stroke.

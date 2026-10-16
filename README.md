# Linux CUA

Clean-room Linux computer-use engine and model surfaces. No OpenAI binaries, JavaScript packages, or instruction text are included. The supplied research specification, public AT-SPI/X11 interfaces, and public provider API documentation informed the implementation.

**Status: working X11 prototype, not a completed Codex-equivalence claim.** Tested with a GTK3 fixture on a private Xvnc display. The MCP server and both provider adapters have exercised the real engine. Live Claude/GPT inference, Codex reference timings, XWayland, and broad application compatibility remain unverified. Native Wayland is not implemented.

## Build

Requirements: current stable Rust, Node ≥22.13, an explicitly owned X11 display with Composite, DAMAGE, XFIXES, XInput2 and X-Resource ≥1.2, and a private D-Bus/AT-SPI session. MIT-SHM is optional. Fixture tests additionally use Python3/PyGObject GTK3 and xdotool. There is no dependency on agent-desktop in the engine or surface.

```sh
cargo build --release --locked
npm ci --prefix surface
cargo test --locked
npm test --prefix surface
```

On this workstation build artifacts were placed on the HDD with `CARGO_TARGET_DIR=/data/scratch/linux-cua-build/target CARGO_BUILD_JOBS=2`; set `CUA_ENGINE` accordingly.

## Run on a private desktop

Bring your own private display/session. Never point this at another agent's display. Display 0 and the default owner session/a11y bus paths are refused. No implicit `$DISPLAY` fallback exists.

Example using an installed agent-desktop (creates only this task's workspace; does not modify agent-desktop):

```sh
W=$(agent-desktop new --label 'Linux CUA' --geometry 1024x768 --a11y)
eval "$(agent-desktop env "$W")"
export CUA_DISPLAY="$DISPLAY"
export CUA_ENGINE="$PWD/target/release/linux-cua"
agent-desktop run "$W" -- /usr/bin/python3 "$PWD/bench/fixture.py"
node surface/bin/linux-cua-mcp.js
# After the client exits, close only this workspace:
agent-desktop close "$W"
```

Configure any MCP stdio client to launch `node /absolute/path/surface/bin/linux-cua-mcp.js` with `CUA_ENGINE`, `CUA_DISPLAY`, `XAUTHORITY`, the private `DBUS_SESSION_BUS_ADDRESS`, and optionally `AT_SPI_BUS_ADDRESS`. The client must support form elicitation, or apps must be explicitly allowed by the trusted operator. Denials always win. Nothing is installed into your Amp/Claude configuration automatically.

## Live demos on your own desktop (explicit opt-in)

The daemon refuses display `:0` unless it is started with an explicit command-line opt-in. Environment variables cannot enable it, so it is never inherited from the MCP surface (which also refuses `:0` itself). Only a trusted host that you start, such as a demo bridge, can pass it:

```sh
CUA_DISPLAY=:0 linux-cua --live-desktop --allow-app gnome-calculator [--allow-app APP_ID ...] \
  (--pid PID | --allow-exe /usr/bin/gnome-calculator) [...]
```

- `--allow-app` (required, repeatable) is the exact, case-insensitive WM_CLASS app ID. Windows of other apps are never listed, read or controlled.
- At least one `--pid` or `--allow-exe` (repeatable) is required, because any client can set any WM_CLASS. A window is admitted only if its process is one of the `--pid`s or its `/proc/<pid>/exe` is one of the `--allow-exe` paths (canonicalized; checked when the window is listed).
- Window-to-process binding uses the X server's own record of the client that created each window (X-Resource `QueryClientIds`, `LocalClientPID`), never the client-writable `_NET_WM_PID`. Windows whose client PID the server cannot report (remote clients) are not listed.
- The per-app approval, deny lists and hard denies still apply. The AT-SPI bus check is unchanged: the owner's default session and accessibility buses are refused, so semantic actions work only for apps started on a private bus (as the demo bridge does); coordinate input and capture work either way.
- The former `CUA_AUTHORIZED_WINDOW_IDS`/`CUA_AUTHORIZED_PID` variables are rejected at startup.

## One engine, three surfaces

- **MCP JS:** persistent QuickJS/WASM context in a separate restricted process. `js {title,code,timeout_ms}`, `js_reset`, `turn_ended`. Top-level await and bindings persist. No model filesystem, shell, module loader, or network API.
- **MCP JSON:** discrete window/AX/capture/action tools over the same daemon.
- **Provider adapters:** `AnthropicComputerToolset` for `computer_toolset_20260801`, `OpenAIComputerTool` for `{type:"computer"}`. Host application owns inference and human approval. See [surface usage](surface/README.md).

```js
const app = await cua.getApp("xed");                     // main window; or {appId}, {windowId}
await app.getAXState({disableDiffing: true});            // emitted into the cell output
// Use an element ID from that observation, not a guessed ID:
await app.setValue(observedFieldId, 'Grüße λ');
await app.click(observedButtonId);   // the cell output ends with the state after the last action
const png = await app.getScreenshot({emit: false});      // PNG bytes (Uint8Array)
await nodeRepl.emitImage(png);
nodeRepl.write('done');
```

The JS API mirrors the names and shapes of OpenAI's computer-use JavaScript API, because GPT models are trained on it: `WindowInfo {id, app, title?}`, `AppInfo {id, displayName?, isRunning?, windows?}`, `cua.getState`, `listApps`, `listWindows`, `getApp({windowId})`, and App methods `getAXState`, `getScreenshot`, `getAXStateAndScreenshot`, `click`, `doubleClick`, `drag`, `scroll`, `typeText`, `pressKey`, `paste`, `setValue`, `performSecondaryAction`. Methods also accept GPT's single-object forms (`click({element})`, `click({x, y})`, `typeText({text})`, `pressKey({key})`, `pressKey(["ctrl", "s"])`, `setValue({element, value})`, `scroll({x, y, scrollY})`, ...); key names are case-insensitive (`Ctrl+s`, `Enter`).

An App covers all top-level windows of its process: dialogs (`WM_TRANSIENT_FOR`), modal windows and popups. `cua.getApp("xed")` picks the app's main window. Observations show the app's focused or modal window, plus a one-line list of its other windows. Element IDs stay valid across those windows, and actions on an element inside a dialog just work. Keys and text go to the app's focused window. Actions observe implicitly when needed, so one cell can open a dialog, fill it, confirm and verify.

`typeText`/`paste` insert through the focused AT-SPI EditableText. When there is none, they fall back to key events to the window's focus. A mismatch, for example a decimal comma locale turning "125.50" into "12550", is reported as a warning with a hint and does not throw. `getAXState({query: "..."})` returns only the matching elements with their ancestors and descendants. `window_id` exists only on the raw NDJSON side and in the discrete MCP tools. Observation and discovery calls emit their result into the cell output unless called with `{emit: false}`; `nodeRepl.write(value)` and `await nodeRepl.emitImage(image)` add output explicitly (`console.log` and `display` are aliases). `getAXState` returns the changes since the tree the model last received for that window (the last emitted observation), never since an internal observation in the same cell; `{disableDiffing: true}` returns the full tree. Coordinates are window-local pixels. `scroll(...,{pixels:n})` is explicit; a numeric distance uses 500 pixels per page. Top-level `const` bindings are mutable across REPL cells. Rich clipboard formats and optional held-key/duration parameters are rejected.

The Rust daemon uses NDJSON stdin/stdout (see [RPC contract](CONTRACT.md)). Actions serialize to keep observations and input ordered. AT-SPI reads inside a snapshot are pipelined. Bulk `Cache.GetItems` is invalidated by events with bounded live traversal for missing caches. Element IDs come from one counter for the whole session and are never reused, so an ID from an old observation can never resolve to a different element. When the host restarts the daemon (timeout, cancellation), it passes `--first-element-id` past the last ID it saw. IDs are window-bound and revalidated before actions. An observation's AT-SPI work is bounded (500 ms budget, 300 ms probe per app). An app that does not answer is reported and skipped for 3 s rather than paying the timeout on every action. Raw results include the full text; MCP results (JS and discrete tools) show the full tree or, when shorter, the changes since the tree the model last received for that window.

X11 SendEvent input targets a window without pointer warps, XTEST, or foreground activation. Coordinate input can also hit the window's own popups (menus, dropdowns, tooltips: override-redirect windows of the same X client); a point over a subwindow owned by another client (XEmbed or reparented windows) is refused. Semantic actions use AT-SPI. A window is redirected with Composite as soon as it is approved or first read, and the first read waits (up to 300 ms) for the client's repaint, so a fresh backing pixmap never shows pixels of overlapping windows; resizes are handled the same way. Capture reads the backing pixmap with MIT-SHM when supported, otherwise GetImage, and composites the client's mapped popups on top. It refuses an occlusion-sensitive capture if Composite fails. A virtual cursor overlay tracks coordinate actions. Settling has two phases: wait up to 100 ms for the first sign the app reacted (damage on the window or its popups, a popup mapping, or an AT-SPI event of the app), then until that activity has been quiet for 35 ms, capped at 600 ms. If nothing reacted, the observation carries a warning. This is a heuristic, not proof a network request completed.

## Safety boundaries

- `CUA_ALLOW_APPS` / `CUA_DENY_APPS`: comma-separated exact, case-insensitive WM_CLASS app IDs. Apps not on the allow list require MCP approval before any window content is read or controlled. Approval covers the app's process, so its dialogs too. The user chooses the scope in the approval form: this turn, this session (until the MCP server exits), or always. "Always" decisions are stored in `CUA_APPROVALS_FILE` (default `$XDG_CONFIG_HOME/linux-cua/approvals.json`, `{"always_allowed": [{"app_id": "Xed", "executable": "/usr/bin/xed"}]}`). An entry matches only that app ID together with that executable. The host reads the file once at startup, so edits (by the operator or by any process running as the same user) apply to later sessions only. Only the trusted host writes it, after an explicit user choice. Listing window metadata does not require approval.
- Terminal/password-manager/security-prompt identifiers and executable names are hard-denied, even if allowed or approved. The executable is resolved from the X-Resource-verified client PID; a window whose process or executable cannot be established is denied, as are unknown app identities. **WM_CLASS can still be chosen freely by a client; this is not an isolation boundary against malicious native clients.** Embedded terminal widgets and browser-hosted password managers are not comprehensively classified. Use a private desktop with only trusted fixture/apps, not your personal desktop or secrets.
- XI2 key/button/motion/touch input latches an abort. The engine checks before input, during semantic operations and long X11 actions, and after settling. A trusted `turn_ended` resets it and approvals. Display-wide abort is intentionally more conservative than window-only abort.
- Confirmation-policy text is original and advisory; it is not a transaction-risk classifier. A separate reviewer model is not implemented. Ask before external or irreversible actions.
- Cancelling or timing out kills the issuing daemon and stops further dispatch. **Events already sent to an app cannot be recalled; the outcome may be partial or unknown.** Never automatically replay an interrupted action. Turn end also cancels active JS cells and queued old-turn requests.
- Raw NDJSON is a trusted-host API, including `approve_app`; never expose it directly to model code or over an unauthenticated network socket. The model's only bridge goes through host validation and approval.
- Bus addresses are checked after percent-decoding and path canonicalization; implicit (`runtime=`, `dir=`, `tmpdir=`, autolaunch), non-unix and abstract-socket addresses are refused (`CUA_ALLOW_ABSTRACT_BUS=1` accepts abstract sockets).
- The daemon's own 20 s request timeout is best effort because X11 waits block its thread. The real request deadline is the trusted host, which kills the daemon (SIGKILL) on timeout or cancellation.

## Verification and measurements

With the private fixture running and the same environment inherited:

```sh
node bench/integration.mjs
python3 bench/run.py --command "$CUA_ENGINE" --iterations 20 \
  --output bench/results/local.json --screenshot bench/results/fixture.png
```

The integration suite exercises the real MCP server, persistent App API, elicitation, Unicode insertion/paste, semantic and coordinate actions, unchanged real pointer/focus, repeated shared-memory captures, turn reset, XI2 abort, and both provider screenshot adapters. For occlusion coverage launch `bench/fixture.py --guard` on the same private display, then run with `CUA_TEST_GUARD=1`. `CUA_TEST_SCREENSHOT=/absolute/path.png` saves the engine capture.

`bench/run.py` checks approval gates and each task's resulting field/status text, and records per-call time, settle/observation time, response bytes, and per-task wall time. It measures local engine operations, **not model latency or actual text-token usage**. It accepts another NDJSON-compatible command for comparison; it does not execute or bundle Codex. `bench/compare.py candidate.json reference.json` refuses incompatible suites, models, dimensions, task counts or failed runs. A reference report must contain actual timings and independently checked outcomes, not the spec's fixed settle constants.

`bench/model.mjs` provides bounded native-provider inference loops on the synthetic fixture. Set the relevant `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` securely in the environment, then select an explicit provider/model. This makes billable API requests and transmits fixture screenshots; subscription CLI OAuth sessions are not treated as API keys. It logs returned token usage, model/tool time, response sizes and independently checked task outcomes, never credentials. No automatic API retries occur. A dry run validates local setup without network calls and is deliberately **not** marked successful performance evidence:

```sh
node bench/model.mjs --provider anthropic --model claude-opus-5-5 \
  --iterations 5 --output bench/results/claude.json --dry-run
node bench/model.mjs --provider openai --model gpt-6.1-sol \
  --iterations 5 --output bench/results/gpt.json --dry-run
# Remove --dry-run only when intending to run paid inference.
```

To establish the requested speed target, run equivalent task suites through Claude, GPT, and an authorized Codex installation, recording model/tool time and completion rates separately. No such reference or live inference measurements were available in this implementation run; performance parity remains unproven.

## Remaining compatibility limits

- Native Wayland needs a portal/libei/PipeWire backend behind the existing RPC. XWayland should expose the X11 path but has not been tested.
- Some apps ignore SendEvent. Keyboard input currently targets the top-level window; child-window focus routing and toolkit coverage need expansion. Verify state after every action.
- `typeText`/`paste` require text. They never read or replace the clipboard. Key-event fallback can type only characters on the display's keyboard map.
- GTK menu items are clicked with the pointer, not AT-SPI DoAction. A menu item that opens a modal dialog (`gtk_dialog_run`) would otherwise run the dialog's nested main loop inside the accessibility call, which blocks the app's AT-SPI connection. While an app's AT-SPI connection is blocked, key and text input to it are refused with `input_refused`: GTK's key snooper makes a synchronous accessibility call per key event, and with a blocked connection the app would hang for good.
- Chromium-based browsers need `--force-renderer-accessibility`. Web inputs expose Text but no EditableText, so `setValue` on them focuses the field with the pointer, selects all and types key events. Containers with more than 1000 children show only the children in view; scroll for more.
- Semantic click uses only an action named click/press/activate/toggle/jump/open/select (or selection/focus for selectable and editable elements); otherwise it fails and lists the available actions, which `performSecondaryAction` can run by name.
- Hover, held buttons/keys, modifier-clicks, arbitrary multi-segment drag paths and native provider full-desktop coordinates are not supported. Adapters reject or disable unsupported operations. `click_button` and `click_count` can be enabled through `CUA_ENGINE_EXTENSIONS` for the supported X11 implementation.
- AT-SPI cache invalidation refetches bulk data; live text/value/geometry still costs per-node calls. Large trees are bounded/truncated with warnings. App mapping uses PID plus frame title/size and rejects ties.
- Only mapped windows are listed. Scroll pixels approximate wheel notches. Screen text fences label untrusted data but do not solve prompt injection.

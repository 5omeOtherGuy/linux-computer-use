# Linux CUA

Linux desktop toolkit for computer-use agents: isolated X11 workspaces, a Rust X11/AT-SPI engine and an MCP surface.

Status: prototype. An exploratory, unreleased project: commands, interfaces and install steps can change without notice.

[![gate](https://github.com/5omeOtherGuy/linux-computer-use/actions/workflows/gate.yml/badge.svg?branch=main)](https://github.com/5omeOtherGuy/linux-computer-use/actions/workflows/gate.yml)
[![licence: MIT](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)

## What it does

Agents get private, throwaway X11 desktops (agent-desktop), a computer-use engine that reads and drives their apps through AT-SPI and X11 without touching the owner's display, and model-facing surfaces (MCP and provider adapters). The engine is a clean-room implementation: no OpenAI binaries, JavaScript packages, or instruction text are included. The supplied research specification, public AT-SPI/X11 interfaces, and public provider API documentation informed the implementation.

Tested with a GTK3 fixture on a private Xvnc display. The MCP server and both provider adapters have exercised the real engine. Live Claude/GPT inference, Codex reference timings, XWayland, and broad application compatibility remain unverified. Native Wayland is not implemented.

| Path | Component |
|---|---|
| `desktop/` | agent-desktop CLI, private workspace lifecycle, native/browser controllers, panel tools and tests |
| `desktop/skill/` | agent instructions and references; the single source for the installed skill |
| `src/` | Rust X11/AT-SPI computer-use engine |
| `surface/` | MCP interfaces and provider adapters |
| `benchmarks/` | end-to-end agent workloads using the bundled desktop manager |

### One engine, three surfaces

- **MCP JS:** persistent QuickJS/WASM context in a separate restricted process. `js {title,code,timeout_ms}`, `js_reset`, `turn_ended`. Top-level await and bindings persist. No model filesystem, shell, module loader, or network API.
- **MCP JSON:** discrete window/AX/capture/action tools over the same daemon.
- **Provider adapters:** `AnthropicComputerToolset` for `computer_toolset_20260801`, `OpenAIComputerTool` for `{type:"computer"}`. Host application owns inference and human approval. See [surface usage](surface/README.md).

```js
const app = await cua.getApp("xed");   // focused main window; or {appId}, {windowId}. Emits its state
// Use an element ID from that observation, not a guessed ID:
await app.setValue(observedFieldId, 'Grüße λ');
await app.click(observedButtonId);   // the cell output ends with the state after the last action
const png = await app.getScreenshot({emit: false});      // PNG bytes (Uint8Array)
await nodeRepl.emitImage(png);
nodeRepl.write('done');
```

The JS API mirrors the names and shapes of OpenAI's computer-use JavaScript API, because GPT models are trained on it: `WindowInfo {id, app, title?}`, `AppInfo {id, displayName?, isRunning?, windows?}`, `cua.getState`, `listApps`, `listWindows`, `getApp({windowId})`, `cua.launchApp(name)` (starts an installed application and returns its App once its window is up), `cua.clipboard.write/read/release`, `cua.help()` (prints the API description again, for a model that lost its context), and App methods `getAXState`, `getScreenshot`, `getAXStateAndScreenshot` (one engine request for both), `click`, `doubleClick`, `rightClick`, `moveMouse`/`hover`, `moveMouseRelative`, `drag`, `scroll`, `typeText`, `pressKey`, `paste`, `setValue`, `performSecondaryAction`. `click`, `drag` and `scroll` take `{modifiers: "shift+ctrl"}` (or an array) for held modifier keys; `click` and `pressKey` take `{durationMs}` to hold the button or key. Methods also accept GPT's single-object forms (`click({element})`, `click({x, y})`, `typeText({text})`, `pressKey({key})`, `pressKey(["ctrl", "s"])`, `setValue({element, value})`, `scroll({x, y, scrollY})`, ...); key names are case-insensitive (`Ctrl+s`, `Enter`).

For continuous canvas strokes, `await app.drag({path: [[100,100], [180,100], [180,180], [100,100]]})` holds the left button through every point (2–256 integer window-local points). Use the app's pencil or brush for arbitrary outlines; shape tools interpret a drag according to their own rules. The engine checks the complete path before pressing, releases on interruption, and settles once after release. The original `drag(from, to)` remains available. Group predictable strokes in one JS cell and inspect a screenshot afterward.

An App covers all top-level windows of its process: dialogs (`WM_TRANSIENT_FOR`), modal windows and popups. `cua.getApp("xed")` picks the app's focused main window, names its other main windows, and emits the app's state, so no `getAXState()` is needed after it (one in the same cell before any action is not emitted again). An unknown name fails with the list of running app IDs, so `listApps()` is not needed first. Observations show the app's focused or modal window, plus a one-line list of its other windows. Element IDs stay valid across those windows, and actions on an element inside a dialog just work. Keys and text go to the app's focused window. Actions observe implicitly when needed, so one cell can open a dialog, fill it, confirm and verify.

`setValue` commits a spin button through AT-SPI Value when available, including range validation and numeric readback; changing only its editable text can leave the application using the old number. Other editable fields keep text insertion and readback.

`typeText`/`paste` insert through the focused AT-SPI EditableText. In a single-line field one trailing newline becomes Return after the insertion; a newline inside the text is refused with a hint. Without a focused EditableText: in a dialog, the text goes into its only text field, otherwise it is refused with `input_refused` (always in an alert or a dialog whose tree is unknown), so that text meant for another window cannot answer an unexpected "Save changes?"; in a main window, and only when that window is the one the model last saw, `typeText` types key events and `paste` pastes through the clipboard (Ctrl+V, so text the keyboard map cannot type still arrives), both read back where the app reports the focused text. `cua.launchApp("drawing")` starts an installed application by its desktop entry ID (`org.gnome.Calculator`), the ID's last part, its menu name or its program name, never a command line; terminal applications, the deny list and the hard denies apply, and the new app still needs the usual approval before it is read or controlled. `cua.clipboard.write(text)` takes the desktop's clipboard (served by the daemon on its own X connection until an app takes it over), `read()` returns its text or `null`, `release()` gives it up. A mismatch from `typeText` or `setValue`, for example a decimal comma locale turning "125.50" into "12550", is reported as a warning with a hint and does not throw. Advice that saves a turn starts with `hint: ` and is shown once per session the first time it reaches the model: apps with number input (spin buttons, an editable display) get the decimal-comma hint in their first observation when `LC_NUMERIC` uses one, and a file manager item view that exposes no items over AT-SPI (Thunar's icon view) suggests the detailed list view (Ctrl+2), whose rows are clickable table cells. `getAXState({query: "..."})` returns only the matching elements with their ancestors and descendants. Wherever an element ID is accepted (`click`, `setValue`, `performSecondaryAction`, `scroll`, `moveMouse`), a locator `{role, name, text, states}` names the element instead: it is resolved against a fresh observation of the app's active window when the action runs, waits up to 1 s for a match, and fails with `element_not_found`, or `ambiguous_element` listing the candidates, unless exactly one element matches. Roles accept common spellings (`push button`, `textbox`, `checkbox`); `name` matches the accessible name exactly (case-insensitive) before partially, or as a RegExp; `text` matches name, description, text or value. `app.find(locator, {timeoutMs})` returns the matches without emitting them. A keyboard shortcut (a Ctrl, Alt or Super chord other than the editing chords, or a function key) and a button click wait 150 ms of quiet instead of 35 ms, so the next action of a cell reaches the dialog they open. `window_id` exists only on the raw NDJSON side and in the discrete MCP tools. Observation and discovery calls emit their result into the cell output unless called with `{emit: false}`; `nodeRepl.write(value)` and `await nodeRepl.emitImage(image)` add output explicitly (`console.log` and `display` are aliases). `getAXState` returns the changes since the tree the model last received for that window (the last emitted observation), never since an internal observation in the same cell; `{disableDiffing: true}` returns the full tree. Coordinates are window-local pixels of the window of the last screenshot or tree model code received (`frame_window` on the raw side). If the app's active window changed since, for example because the previous click opened a dialog, coordinate input is refused with `input_refused` until the model looks again; if that window closed, it fails with `window_not_found`. `scroll(...,{pixels:n})` is explicit; a numeric distance uses 500 pixels per page. Top-level `const` bindings are mutable across REPL cells. Rich clipboard formats are rejected; the clipboard carries plain text only.

The Rust daemon uses NDJSON stdin/stdout (see [RPC contract](CONTRACT.md)). Actions serialize to keep observations and input ordered. AT-SPI reads inside a snapshot are pipelined. The daemon keeps a mirror of each app's objects: the bulk `Cache.GetItems` result plus everything fetched live since (dialog widgets and tree-view cells that atk-bridge never caches). An AT-SPI event names the objects it touches, and only those (with their children) are fetched again at the next observation; the mirror is rebuilt after 5 s as a backstop. The window's accessible frame, once mapped, is reused with a single call instead of enumerating the app's top-levels. Element IDs come from one counter for the whole session and are never reused, so an ID from an old observation can never resolve to a different element. When the host restarts the daemon (timeout, cancellation), it passes `--first-element-id` past the last ID it saw. IDs are window-bound and revalidated before actions. An observation's first call waits up to 1 s for a busy app (a file chooser populating after it opened), because an observation without a tree costs the model a turn; an app confirmed blocked inside an earlier accessibility action gets 300 ms. The tree walk after that has a 500 ms budget; inner call timeouts derive from that deadline. A `Cache.GetItems` that times out is not retried for 10 s and a deadline-bounded live walk returns a partial tree with a warning instead; the previous element map stays valid until a new snapshot succeeds. An app that does not answer is reported and later re-probed briefly (60 ms) rather than paying the full wait on every action. An action with `observe: false` returns the window it landed on and the app's windows without a tree: the MCP surface sends it for every action in a `js` cell and observes once when the cell ends, and the provider adapters send it because they capture a screenshot after the batch. `screenshot: true` on an observation or action attaches the window's image to the same result (`getAXStateAndScreenshot` is one request). Raw results include the full text; MCP results (JS and discrete tools) show the full tree or, when shorter, the changes since the tree the model last received for that window.

X11 SendEvent input targets a window without pointer warps, XTEST, or foreground activation. Coordinate input can also hit the window's own popups (menus, dropdowns, tooltips: override-redirect windows of the same X client); a point over a subwindow owned by another client (XEmbed or reparented windows) is refused. Held modifiers (`modifiers`) are pressed as key events to the window before the pointer events and carried in their state; `hold_ms` keeps a button or key down (physical input still aborts, and the release is always sent); `mouse_move` hovers (enter/leave and motion without a button; GTK shows tooltips about 0.5 s later). Semantic actions use AT-SPI. A window is redirected with Composite as soon as it is approved or first read, and the first read waits (up to 300 ms) for the client's repaint, so a fresh backing pixmap never shows pixels of overlapping windows; resizes are handled the same way. Capture reads the backing pixmap with MIT-SHM when supported, otherwise GetImage, and composites the client's mapped popups on top. It refuses an occlusion-sensitive capture if Composite fails. A virtual cursor overlay tracks coordinate actions. Settling has two phases: wait up to 200 ms for the first sign the app reacted (damage on the window or its popups, a popup mapping, a top-level window being created, or an AT-SPI event of the app; a GTK app building a file chooser shows nothing for 70-100 ms on an idle machine), then until that activity has been quiet for 35 ms, capped at 600 ms; while the app has reacted only with accessibility events and shown nothing yet (it is building a dialog, with pauses), the quiet interval is 150 ms. If nothing reacted, the observation carries a warning. Settling also waits (up to 1.5 s) for a top-level window the settle target's client is creating to map (windows already mapped when the wait starts are skipped), and after Return, Enter or Escape, a button click, or text ending in a newline inside a dialog, for that dialog to close: up to 1.5 s from the action, ending 350 ms after the dialog's last X or AT-SPI activity, or after 1 s if it showed none (the GTK file chooser opens a typed file 400-900 ms after Return with no activity before it unmaps, the longer on a loaded machine), so a dialog that stays open after a navigation (a file chooser entering a typed folder) costs about 0.45 s; a dialog that closes after a longer pause would cost the model a turn, which is why the waits err on the patient side. A dialog whose accessible frame is already gone when it is observed is closing: the observation waits up to 300 ms for it to unmap and shows the window behind it. So the next action in a cell reaches the dialog that just opened, or the window behind a dialog that just closed. This is a heuristic, not proof a network request completed.

## Install

Requirements: current stable Rust, Node ≥22.13, an explicitly owned X11 display with Composite, DAMAGE, XFIXES, XInput2 and X-Resource ≥1.2, and a private D-Bus/AT-SPI session. MIT-SHM is optional. Fixture tests additionally use Python3/PyGObject GTK3 and xdotool. There is no dependency on agent-desktop in the engine or surface.

```sh
git clone https://github.com/5omeOtherGuy/linux-computer-use
cd linux-computer-use
cargo build --release --locked
npm ci --prefix surface
cargo test --locked
npm test --prefix surface
```

Then link the desktop commands and skill from a release or dedicated installation checkout. Links follow that checkout, so installed tools should not point to a worktree being edited:

```sh
python3 scripts/install-desktop.py
# To migrate existing command/skill symlinks from a separate checkout:
python3 scripts/install-desktop.py --replace-links
```

The installer creates symlinks under `~/.local/bin` and `~/.agents/skills/agent-desktop`; it never replaces regular files or installs packages. The optional owner-facing panel remains in `desktop/panel/`. The MCP server and benchmark harness prefer the bundled CLI; `CUA_AGENT_DESKTOP` overrides that choice. The engine can still use another explicitly owned private X11 session independently.

With a custom `CARGO_TARGET_DIR` (for example on a larger disk) and `CARGO_BUILD_JOBS=2` on small machines, set `CUA_ENGINE` to the built binary accordingly.

## Usage

Bring your own private display/session. Never point this at another agent's display. Display 0 and the default owner session/a11y bus paths are refused. No implicit `$DISPLAY` fallback exists.

Example using the bundled agent-desktop commands after installation:

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

Configure any MCP stdio client to launch `node /absolute/path/surface/bin/linux-cua-mcp.js` with `CUA_ENGINE`, `CUA_DISPLAY`, `XAUTHORITY`, the private `DBUS_SESSION_BUS_ADDRESS`, and optionally `AT_SPI_BUS_ADDRESS`. On a workstation with agent-desktop, `CUA_WORKSPACE=new` replaces those: the server creates a private workspace with accessibility (`CUA_WORKSPACE_LABEL`, `CUA_WORKSPACE_GEOMETRY`), owned by the server process and closed when it exits, and apps started with `cua.launchApp` run in it; `CUA_WORKSPACE=NAME` attaches to an existing workspace instead and leaves it open. The server uses `desktop/bin/agent-desktop` from this checkout, falling back to `PATH` or `~/.local/bin` for a standalone surface installation (MCP clients such as Codex start servers with a reduced environment); `CUA_AGENT_DESKTOP` sets its path explicitly. The client must support form elicitation, or apps must be explicitly allowed by the trusted operator. Denials always win. Nothing is installed into your Amp/Claude configuration automatically.

### Live demos on your own desktop (explicit opt-in)

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

## Safety boundaries and configuration

- `CUA_ALLOW_APPS` / `CUA_DENY_APPS`: comma-separated exact, case-insensitive WM_CLASS app IDs. Apps not on the allow list require MCP approval before any window content is read or controlled. Approval covers the app's process, so its dialogs too. The user chooses the scope in the approval form: this turn, this session (until the MCP server exits), or always. "Always" decisions are stored in `CUA_APPROVALS_FILE` (default `$XDG_CONFIG_HOME/linux-cua/approvals.json`, `{"always_allowed": [{"app_id": "Xed", "executable": "/usr/bin/xed"}]}`). An entry matches only that app ID together with that executable; turn and session approvals are keyed the same way, and the daemon additionally binds an approval to the process start time, so a reused PID does not inherit it. A malformed or old-format file is reported on stderr and treated as empty, never overwritten. The host reads the file once at startup, so edits (by the operator or by any process running as the same user) apply to later sessions only. Only the trusted host writes it, after an explicit user choice. Listing window metadata does not require approval.
- `launch_app` starts only installed desktop entries (`Type=Application`, shown in menus), by name, with `Exec` field codes expanded to nothing; the model never supplies a command line or arguments. Entries with `Terminal=true`, on the deny list or matching the hard denies (by desktop ID, name, program or WM class) are refused, and the started app's windows need the usual approval. It is unavailable on a live desktop, as is the clipboard.
- Terminal/password-manager/security-prompt identifiers and executable names are hard-denied, even if allowed or approved. The executable is resolved from the X-Resource-verified client PID; a window whose process or executable cannot be established is denied, as are unknown app identities. **WM_CLASS can still be chosen freely by a client; this is not an isolation boundary against malicious native clients.** Embedded terminal widgets and browser-hosted password managers are not comprehensively classified. Use a private desktop with only trusted fixture/apps, not your personal desktop or secrets.
- XI2 key/button/motion/touch input latches an abort. The engine checks before input, during semantic operations and long X11 actions, and after settling. A trusted `turn_ended` resets it and approvals. Display-wide abort is intentionally more conservative than window-only abort.
- Confirmation-policy text is original and advisory; it is not a transaction-risk classifier. A separate reviewer model is not implemented. Ask before external or irreversible actions.
- Cancelling or timing out kills the issuing daemon and stops further dispatch. **Events already sent to an app cannot be recalled; the outcome may be partial or unknown.** Never automatically replay an interrupted action. Turn end also cancels active JS cells and queued old-turn requests.
- Raw NDJSON is a trusted-host API, including `approve_app`; never expose it directly to model code or over an unauthenticated network socket. The model's only bridge goes through host validation and approval.
- Bus addresses are checked after percent-decoding and path canonicalization; implicit (`runtime=`, `dir=`, `tmpdir=`, autolaunch), non-unix and abstract-socket addresses are refused (`CUA_ALLOW_ABSTRACT_BUS=1` accepts abstract sockets).
- The daemon's own 20 s request timeout is best effort because X11 waits block its thread. The real request deadline is the trusted host, which kills the daemon (SIGKILL) on timeout or cancellation.

## Compatibility

Last verified 2026-10-18: the display-free checks in `scripts/gate` on Ubuntu 24.04 with Rust 1.99.1, Node 24 and the system Python 3. The X11 behaviour was last measured on 2026-10-16 on an XFCE/X11 workstation with private Xvnc displays (see [measurements](docs/measurements.md)).

- Native Wayland needs a portal/libei/PipeWire backend behind the existing RPC. XWayland should expose the X11 path but has not been tested.
- Some apps ignore SendEvent. Keyboard input currently targets the top-level window; child-window focus routing and toolkit coverage need expansion. Verify state after every action.
- `typeText`/`paste` require text. `typeText` never touches the clipboard; its key-event fallback can type only characters on the display's keyboard map. `paste` without a text field to insert into replaces the clipboard with the text and presses Ctrl+V, so it reaches only apps bound to that shortcut.
- Element clicks are pointer clicks at the element's visible centre (clipped by its ancestors) whenever it has geometry and the toolkit's hit test does not report another element covering that point (an overlay, a sticky header); AT-SPI DoAction is used without geometry or when covered. When the toolkit's hit test stops at a container or answers with a sibling (GTK's scrolled window answers nothing for a tree view's rows, and GTK tree views leave the header row out of their coordinates), an element of a table is resolved by the table's own row geometry through the AT-SPI Table interface, and only another row at that point counts as a cover; anything else does not stop the click. A button or menu item that opens a modal dialog (`gtk_dialog_run`) would otherwise run the dialog's nested main loop inside the accessibility call, which blocks the app's AT-SPI connection. A menu item without geometry is refused, and a menu item's click must land on one of the app's popups (an item scrolled out of a long menu is refused rather than clicked in the window behind). While an earlier accessibility action call to an app has timed out and not returned, key and text input to it are refused with `input_refused`: GTK's key snooper makes a synchronous accessibility call per key event, and with a blocked connection the app would hang for good. A merely slow app is re-probed for up to 1 s and then gets the input with a warning.
- Chromium-based browsers need `--force-renderer-accessibility`. Web inputs expose Text but no EditableText, so `setValue` on them focuses the field with the pointer, checks that the click focused a writable field (else it fails without typing), selects all and types key events. Containers with more than 1000 children show only the children in view; scroll for more.
- Semantic click uses only an action named click/press/activate/toggle/jump/open/select (or selection/focus for selectable and editable elements); otherwise it fails and lists the available actions, which `performSecondaryAction` can run by name.
- Hover, right/middle and multi-clicks, held modifiers and press durations are part of the contract (`CUA_ENGINE_EXTENSIONS` is accepted for compatibility and changes nothing). Separate button press and release across calls (`left_mouse_down`/`left_mouse_up`) and native provider full-desktop coordinates are not supported; the adapters disable or reject them.
- The object mirror relies on apps announcing changes with AT-SPI events (names, states, children); a change without an event is seen at the latest after the 5 s rebuild. Text, value and geometry are always read live, per node. Large trees are bounded/truncated with warnings. App mapping uses PID plus frame title, size and position (a frame that encloses the window within decoration-sized margins) and rejects ties.
- Only mapped windows are listed. Scroll pixels approximate wheel notches. Screen text fences label untrusted data but do not solve prompt injection.

## Development

`scripts/gate` runs everything a change must pass (formatting, lint, Rust, Node and Python tests, syntax checks and a secret scan) without a display, credentials or your home directory. It needs Rust with rustfmt and clippy, Node ≥22.13 and Python 3 with `websockets`. Focused tests:

```sh
cargo test --locked x11::tests::display_zero_denied
npm test --prefix surface
```

Desktop and installer checks that do not connect to a display:

```sh
python3 -m unittest discover -s tests -p test_install_desktop.py -v
python3 -m unittest discover -s desktop/tests -p test_ownership.py -v
python3 -m unittest discover -s desktop/tests -p test_native.py -v
python3 -m unittest discover -s desktop/tests -p test_browser.py -v
```

`python3 -m unittest discover -s desktop/tests -p test_fast.py -v` additionally creates and closes its own private fixture desktop. The legacy full desktop suite includes owner-display queries and panel tests; use the targeted checks above on a shared workstation.

### On a private display

With the private fixture running and the same environment inherited:

```sh
node bench/integration.mjs
python3 bench/run.py --command "$CUA_ENGINE" --iterations 20 \
  --output bench/results/local.json --screenshot bench/results/fixture.png
```

The integration suite exercises the real MCP server, persistent App API, elicitation, Unicode insertion/paste, semantic and coordinate actions, unchanged real pointer/focus, repeated shared-memory captures, turn reset, XI2 abort, and both provider screenshot adapters. `surface/test/real-apps.test.js` is an opt-in regression suite against real apps (xed, gnome-calculator with `LC_NUMERIC=de_DE.UTF-8`, a launched Drawing) for coordinate refusal after a dialog opens, the dialog-close wait, single-line newlines, key-typing into dialogs, decimal-comma hints, shift-click range selection, hover and relative moves, the one-request tree-plus-screenshot, the clipboard in both directions and `launchApp`: `cd surface && CUA_REAL_ENGINE=/path/to/linux-cua node --test test/real-apps.test.js` on a private display with accessibility. `surface/test/drawing.test.js` is an additional opt-in regression for complete continuous strokes and refusal of an out-of-window path before drawing; run it with the same private-display environment. For occlusion coverage launch `bench/fixture.py --guard` on the same private display, then run with `CUA_TEST_GUARD=1`. `CUA_TEST_SCREENSHOT=/absolute/path.png` saves the engine capture.

`bench/run.py` checks approval gates and each task's resulting field/status text, and records per-call time, settle/observation time, response bytes, and per-task wall time. It measures local engine operations, **not model latency or actual text-token usage**. It accepts another NDJSON-compatible command for comparison; it does not execute or bundle Codex. `bench/compare.py candidate.json reference.json` refuses incompatible suites, models, dimensions, task counts or failed runs. A reference report must contain actual timings and independently checked outcomes, not the spec's fixed settle constants.

`bench/model.mjs` provides bounded native-provider inference loops on the synthetic fixture. Set the relevant `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` securely in the environment, then select an explicit provider/model. This makes billable API requests and transmits fixture screenshots; subscription CLI OAuth sessions are not treated as API keys. It logs returned token usage, model/tool time, response sizes and independently checked task outcomes, never credentials. No automatic API retries occur. A dry run validates local setup without network calls and is deliberately **not** marked successful performance evidence:

```sh
node bench/model.mjs --provider anthropic --model claude-opus-5-5 \
  --iterations 5 --output bench/results/claude.json --dry-run
node bench/model.mjs --provider openai --model gpt-6.1-sol \
  --iterations 5 --output bench/results/gpt.json --dry-run
# Remove --dry-run only when intending to run paid inference.
```

**The standard benchmark** is `benchmarks/run.mjs` with its defaults: GPT-6.1-sol at medium effort in Codex (`codex exec`) draws a house in GNOME Drawing (apt package `drawing`) on a fresh private desktop and saves it; the saved PNG is verified for a non-trivial drawing in at least two colours. Run `node benchmarks/run.mjs --label <change> --iterations N` (add `--record` for a screen recording), then `node benchmarks/report.mjs` for pass rate and median wall, tool-call and tool time per label. The earlier tasks (`reimburse`, `xed-note`) and Claude (`--agent claude --model claude-opus-5-5`) stay available for cross-checks. Results append to `$BENCH_ROOT/results.jsonl` (default `~/.cache/linux-cua-bench`). Wall time includes model inference, which dominates; compare tool time and tool calls for engine changes, and several iterations, because single runs vary by seconds.

Recorded results are in [docs/measurements.md](docs/measurements.md).

## Licence

MIT, see [LICENSE](LICENSE).

# Linux CUA

Clean-room Linux computer-use engine and model surfaces. No OpenAI binaries, JavaScript packages, or instruction text are included. The supplied research specification, public AT-SPI/X11 interfaces, and public provider API documentation informed the implementation.

**Status: working X11 prototype, not a completed Codex-equivalence claim.** Tested with a GTK3 fixture on a private Xvnc display. The MCP server and both provider adapters have exercised the real engine. Live Claude/GPT inference, Codex reference timings, XWayland, and broad application compatibility remain unverified. Native Wayland is not implemented.

## Build

Requirements: current stable Rust, Node ≥22.13, an explicitly owned X11 display with Composite, DAMAGE, XFIXES and XInput2, and a private D-Bus/AT-SPI session. MIT-SHM is optional. Fixture tests additionally use Python3/PyGObject GTK3 and xdotool. There is no dependency on agent-desktop in the engine or surface.

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

## One engine, three surfaces

- **MCP JS:** persistent QuickJS/WASM context in a separate restricted process. `js {title,code,timeout_ms}`, `js_reset`, `turn_ended`. Top-level await and bindings persist. No model filesystem, shell, module loader, or network API.
- **MCP JSON:** discrete window/AX/capture/action tools over the same daemon.
- **Provider adapters:** `AnthropicComputerToolset` for `computer_toolset_20260801`, `OpenAIComputerTool` for `{type:"computer"}`. Host application owns inference and human approval. See [surface usage](surface/README.md).

```js
const windows = await cua.listWindows();
const app = await cua.getApp({windowId: windows[0].window_id});
await app.getAXState({disableDiffing: true});
// Use an element ID from that observation, not a guessed ID:
await app.setValue(observedFieldId, 'Grüße λ');
await app.click(observedButtonId); // returns post-action observation
await app.getScreenshot();        // emitted as an MCP image
```

`cua.getState`, `listApps`, `listWindows`, `getApp` and App methods `getAXState`, `getScreenshot`, `click`, `drag`, `scroll`, `typeText`, `pressKey`, `paste`, `setValue`, `performSecondaryAction` are available. Coordinates are window-local pixels. `scroll(...,{pixels:n})` is explicit; a numeric distance uses 500 pixels per page. `getScreenshot` returns an opaque image handle, not the proprietary API's Uint8Array. Top-level `const` bindings are mutable across REPL cells. `emit` is currently accepted but output is always auto-emitted; rich clipboard formats and optional held-key/duration parameters are rejected.

The Rust daemon uses NDJSON stdin/stdout (see [RPC contract](CONTRACT.md)). Actions serialize to keep observations and input ordered. AT-SPI reads inside a snapshot are pipelined. Bulk `Cache.GetItems` is invalidated by events with bounded live traversal for missing caches. IDs are window-bound and revalidated before actions. Raw results include full text plus diffs; discrete MCP results choose a shorter complete diff, while JS batches emit a full final tree because internal intermediate observations may not have reached the model.

X11 SendEvent input targets a window without pointer warps, XTEST, or foreground activation. Semantic actions use AT-SPI. Capture uses a Composite backing pixmap and MIT-SHM when supported, otherwise GetImage on that pixmap. It refuses an occlusion-sensitive capture if Composite fails. A virtual cursor overlay tracks coordinate actions. XDamage settles actions after a 35 ms quiet interval, capped at 600 ms; this is a heuristic, not proof a network request completed.

## Safety boundaries

- `CUA_ALLOW_APPS` / `CUA_DENY_APPS`: comma-separated exact, case-insensitive WM_CLASS app IDs. An empty allow list requires MCP approval before any window content is read or controlled. Listing window metadata does not require approval.
- Terminal/password-manager/security-prompt identifiers and executable names are hard-denied, even if allowed or approved. Unknown app identities are denied. **X11 metadata can be forged; this is not an isolation boundary against malicious native clients.** Embedded terminal widgets and browser-hosted password managers are not comprehensively classified. Use a private desktop with only trusted fixture/apps, not your personal desktop or secrets.
- XI2 key/button/motion/touch input latches an abort. The engine checks before input, during semantic operations and long X11 actions, and after settling. A trusted `turn_ended` resets it and approvals. Display-wide abort is intentionally more conservative than window-only abort.
- Confirmation-policy text is original and advisory; it is not a transaction-risk classifier. A separate reviewer model is not implemented. Ask before external or irreversible actions.
- Cancelling or timing out kills the issuing daemon and stops further dispatch. **Events already sent to an app cannot be recalled; the outcome may be partial or unknown.** Never automatically replay an interrupted action. Turn end also cancels active JS cells and queued old-turn requests.
- Raw NDJSON is a trusted-host API, including `approve_app`; never expose it directly to model code or over an unauthenticated network socket. The model's only bridge goes through host validation and approval.

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
- `typeText`/`paste` currently require a focused AT-SPI EditableText control; they insert Unicode directly and do not replace the clipboard. `setValue` can target an unfocused field. Pixel-only text input without AX is not wired through the daemon.
- Hover, held buttons/keys, modifier-clicks, arbitrary multi-segment drag paths and native provider full-desktop coordinates are not supported. Adapters reject or disable unsupported operations. `click_button` and `click_count` can be enabled through `CUA_ENGINE_EXTENSIONS` for the supported X11 implementation.
- AT-SPI cache invalidation refetches bulk data; live text/value/geometry still costs per-node calls. Large trees are bounded/truncated with warnings. App mapping uses PID plus frame title/size and rejects ties.
- Only mapped windows are listed. Scroll pixels approximate wheel notches. Screen text fences label untrusted data but do not solve prompt injection.

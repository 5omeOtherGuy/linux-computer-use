---
name: agent-desktop
description: Read before opening, driving or screenshotting any GUI program on this workstation. Private X desktops, screenshot-free native and browser controls, native action batches, visual fallback and cleanup.
---

# agent-desktop

Each workspace is a private Xvnc server (`:100`–`:199`), openbox, D-Bus bus and XDG/runtime dirs. The CLI and this skill ship together under `desktop/` in the Linux CUA repository. Resolve the installed executable with `readlink -f "$(command -v agent-desktop)"`; the owner watches through the panel menu.

## Isolation and lifecycle

- Start GUI programs through `agent-desktop run`; never drive or query the owner's display `:0`. Never run `agent-desktop view`: that opens an owner window.
- Create at most one workspace per task unless the brief says otherwise. Reuse it through the task and close **only the workspace you created** immediately afterward. The CLI enforces creator ownership using the harness PID/start time and Codex thread ID when available; `close --all` selects only that creator's workspaces. Prefer `close NAME` to keep cleanup scoped to the task. Without a recognized harness, ownership binds to the invoking parent process. `--owner-pid` controls automatic cleanup, not close authorization. Older workspaces without creator metadata are refused by `close` and retain automatic cleanup.
- Exit 3 means refusal (ownership, workspace cap or MemAvailable below 1.2 GiB). Report or wait; do not loop. `run --force` is only for tiny programs such as xterm.
- Existing credential authorization applies; never type credentials outside its scope. Private browser profiles have no owner logins. Never use `--share-home` with a browser: single-instance handoff can reach the owner's desktop.
- Screenshots/logs remain in `~/.cache/agent-desktop/ws/NAME/` until close. Idle and owner-session cleanup are fallbacks, not a substitute for closing.

## Computer-use engine

This skill ships with the Linux CUA engine in the same repository. When its MCP tools are available, use them to observe and operate apps; the server uses the bundled desktop manager. Configure `CUA_WORKSPACE=new` to let the server create and close its own private desktop, or `CUA_WORKSPACE=NAME` to attach to a workspace you created and will close yourself. Do not create a second desktop when the server already manages one. The CLI native/browser/batch controls below remain available for shell-based workflows. Keep input to one controller at a time and observe again after switching controllers. See the [toolkit README](../../README.md) for build and MCP configuration.

## Fast workflow

Optimize successful task completion, including model/tool round trips and recovery:

1. Choose an existing authorized app integration when it directly fits the task. For native GUI workflows, create with `--a11y` and read [references/native-control.md](references/native-control.md); inspect native accessible controls before resorting to pixels. Native `find` binds a uniquely observed control locally so a batch can pass through expected dialogs without a model round trip; `select` verifies file selection before cut/copy/rename. For a private browser or Signal renderer, use its structured controls. Create once, usually `--maximize`, then launch the app and use `wait --class/--name/--pid` instead of a guessed startup sleep. Group independent setup commands in one shell call.
2. For web forms and application workflows, prefer structured browser observations and observed element references: read [references/browser-control.md](references/browser-control.md). They expose accessible names, field values, links and row text without screenshots. Browser `find` binds a unique current control to later actions in the same request; use it for known semantic affordances and retain changed-state verification. Use screenshots for canvas, visual layout, missing semantics or uncertain state. For coordinate input, inspect an initial screenshot; never invent controls or act on a stale layout.
3. For native GUI input, prefer **`batch`**, one native X connection for the entire action array. Group predictable actions through the next point where new UI information is needed: shortcut → paste → Enter, or several known form fields using Tab. Ordinary chained CLI commands remain compatible but launch a process per command.
4. Prefer keyboard shortcuts, tab order, search and direct navigation over repeated scrolling and hunting. For bulk text, use `paste`: Unicode arrives through the workspace clipboard. Use `type` when actual key events matter or paste is blocked; ASCII defaults to zero delay in batches, Unicode retains a safety delay. Terminal paste uses `"key":"ctrl+shift+v"`. A paste replaces only this workspace's clipboard; its owner keeps serving asynchronous application requests.
5. Verify browser outcomes from fresh relevant structured state; command completion alone is insufficient. For visual control, `batch` returns a final screenshot path and measured execution time by default. **Open that image** and verify the result; a successful command or printed image path is not visual verification. In a harness supporting image tool results, forward the image in the same orchestration call that runs the batch, avoiding a separate model round trip just to open the path.
6. When the next action depends on navigation, a dialog, autocomplete or asynchronous content, stop and inspect, or wait for a known condition. `wait_window` waits for a visible window; `wait_stable` waits for unchanged pixels and is only a rendering heuristic, not proof a page loaded. Prefer a relevant region when a spinner/video prevents full-screen stability. A fixed sleep is a fallback for a measured application requirement.
7. Reuse successful sequences in scripts when layouts and preconditions are known. Do not parallelize input to the same workspace. If a batch fails, read its completed count and inspect before continuing; never blindly replay a partly completed batch, especially a submission. The failing action itself may already have sent input.

A short local batch does not remove model inference or tool transport latency. Do not promise human-comparison speed or whole-task improvements from a local input benchmark.

## Commands

```bash
W=$(agent-desktop new --label "$TASK" --maximize)
agent-desktop run "$W" -- brave-browser --no-first-run https://example.org
agent-desktop wait "$W" --class brave --timeout 40
agent-desktop screenshot "$W"  # open the returned image before choosing coordinates

agent-desktop batch "$W" <<'JSON'
[
  {"action":"key","keys":["ctrl+l"]},
  {"action":"paste","text":"https://example.org/search?q=hello"},
  {"action":"key","keys":["Return"]}
]
JSON
# Open the final screenshot; navigation may still be in progress.
agent-desktop close "$W"
```

`batch N [JSON]` reads JSON from stdin when omitted. Output: `{completed, elapsed_ms, screenshots, results}`. Full-batch schema validation precedes input; runtime failures report partial completion. `--no-screenshot` is for an intermediate sequence whose result is checked later or by another reliable observation. `--region X,Y,W,H`, `--scale F`, and `-o FILE` control the final screenshot. Screenshots are lossless PNG; keep native resolution for small text and precise targets. With scaling, original coordinates are `image_coordinate/F`; with cropping, also add the region origin.

Action schema and persistent-session recipe: read [references/fast-input.md](references/fast-input.md) when building a batch beyond the example or reusing a runtime.

Standalone commands remain available:

| Command | Purpose |
|---|---|
| `new [--name N] [--label TEXT] [--geometry 1280x800] [--maximize] [--a11y] [--idle MIN]` | Create workspace; prints name |
| `native N [JSON]` | Persistent private accessibility observations and actions; requires `new --a11y` |
| `browser N [JSON] [--app browser\|signal] [--start] [--url URL]` | Structured private browser or Signal renderer controls |
| `run N [--wait] [--timeout S] -- PROGRAM ARGS…` | Launch; prints PID and log; `--wait` returns output/exit code |
| `wait N --name REGEX\|--class REGEX\|--pid P [--timeout 30]` | Visible-window condition |
| `screenshot N [-o FILE] [--window ID] [--region X,Y,W,H] [--scale F]` | Save PNG and print path |
| `paste N TEXT` or `paste N --stdin [--key ctrl+shift+v]` | Bulk UTF-8 clipboard input |
| `type N TEXT [--delay MS]` or `type N --stdin` | Key-event input; standalone default 12 ms, Unicode ≥80 ms |
| `key N KEY…` | Key names: `Return`, `ctrl+l`, `Tab`, `Escape` |
| `click N X Y [--button 1\|2\|3] [--double] [--window ID]` | Click |
| `move N X Y`, `drag N X1 Y1 X2 Y2`, `scroll N up\|down\|left\|right [K] [--at X Y]` | Pointer controls |
| `windows N [--json]`, `focus N ID\|--name REGEX` | Window metadata and focus |
| `list [--json]`, `info N`, `env N` | Workspaces, details, isolated shell exports |
| `close N` | Close own workspace and its processes/files |

Exit codes: 0 success, 1 error, 2 usage/no workspace, 3 refusal, 4 timeout. Program output: the `log` path returned by `run`; server logs: workspace `logs/`.

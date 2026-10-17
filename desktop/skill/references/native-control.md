# Native desktop and assistant workflows

Use AT-SPI for GTK and other desktop applications exposing accessible controls; use the embedded-browser route for the tested Signal desktop app; fall back to visual batches when neither exposes usable state. Prefer an existing authorized application integration for sending messages or managing data when the task does not require GUI interaction. A fast integration and a fast GUI controller are different measurements.

## Private native controls

```bash
W=$(agent-desktop new --label "Assistant workflow" --maximize --a11y)
agent-desktop run "$W" -- xed --new-window
agent-desktop native "$W" '[{"op":"wait","app":"xed","query":"frame"},{"op":"inspect","app":"xed","query":"text|button","limit":40}]'
```

`--a11y` enables accessibility on the workspace's private D-Bus bus. The launcher returns `program` and `pid`; an app lookup accepts the PID string as well as the exact observed app name. Use those identities instead of guessing a calculator/editor implementation. Start applications through `run` so their process marker and environment belong to that workspace. The numeric desktop index inside AT-SPI is independent of X display numbering. The CLI never binds this controller to the owner's desktop. Private workspaces are required; `--share-home` workspaces are refused.

The first native request starts a persistent helper with a Unix socket; later requests reuse its accessibility proxies and native X input connection. No screenshot is taken. The helper checks the workspace process marker before reading or acting, pins references to a backend generation, and matches windows using observed names or bounds within the same app process. Closing the workspace removes its helper and socket. Do not restart the helper and reuse old references.

### Operations

Input is a JSON array as an argument or stdin. A `ref`/`scope` is copied from an observation or uses an earlier request-local `$binding`, for example `n012345abcdef_1`. These are opaque references, not tree indexes. Native operations use `op`; visual batches use `action`.

| Operation | Fields and behavior |
|---|---|
| `find` | `app`, exact `role`, unique `as` name; optional exact `name`, literal `contains` in name/text, `top:true` (only direct application-child windows), `states` (required state names, e.g. `["focused","editable"]`), `scope`, `timeout` (default 3 seconds), `depth` (default 40). Waits for exactly one showing match and returns its observed row. Later operations in the same request use `$name` as a reference/scope. Ambiguous or incomplete traversal stops the batch. Scope must belong to the requested app. |
| `select` | `ref`: exclusively select the item through its nearest native Selection container, verify selection, return its container reference. Focus alone does not select a file. |
| `apps` | List only private workspace applications and their references. |
| `inspect` | Optional `app` exact name or PID string, `query` literal role/name/text substrings (not regex), alternatives separated by `|`, `scope` observed ref, `limit` (default100, maximum500), `depth` (default22, maximum40). Returns names, roles, text, actions, state and references. Traversal is bounded and reports truncation. |
| `read` | `ref`, optional `offset`/`length`: text up to12000 characters by default. Password text is redacted. |
| `fill` | `ref`, `text`: replace enabled editable text through the native accessibility interface. Verify with read or inspection. |
| `paste` | `ref`, `text`: activate the observed window/control and use its private clipboard. Useful for custom app input without an editable-text interface. |
| `invoke` | `ref`, `action`: invoke an action actually listed on that control, such as `click`, `activate` or `show_location`. |
| `focus` | `ref`: activate the containing app window and focus the target where appropriate. |
| `key` | `ref`, `keys`: activate the observed target/window and send sequential shortcuts, such as `["ctrl+s"]`, `["ctrl+shift+n"]`. Each chord is one string. |
| `wait` | `app`, `query`, optional `timeout` (default10 seconds), `limit`: condition-based wait for matching accessible state. |

A chooser can contain a second nested chooser. Use `top:true` when finding the actual window, or use an exact observed name; role alone can be ambiguous.

Use `find` to keep predictable state-dependent work inside one request, such as opening a chooser, finding its observed editable field, filling it, activating the expected button and waiting for the new frame. Bindings are unique and exist only inside that request; returned opaque references can be reused normally. The whole schema and binding order are validated before input. Stop on lookup failure and inspect partial results. A requested timeout is a polling budget; one in-flight accessibility traversal can overrun it.

Choose a compact app query or scope before traversing all applications. Wait for newly launched apps to appear rather than inferring readiness from the launch PID. Batch predictable input until the next real information dependency. Target an editable control for field input, and its frame or chooser for application shortcuts. A nonfocusable result label is not necessarily an input field. Inspect focusable/active/modal state and the available actions.

A dialog can be exposed as a `file chooser` or `alert` rather than a `dialog`. Observe and target the actual chooser/alert. After a file opens, saves or moves, wait for its expected new title/path or confirmation, then read the actual contents. An immediately successful action may precede the UI update.

`native` validates the whole request before input. Output reports `completed`, `results`, `elapsed_ms` and an error on failure. Runtime failures can follow partial input, including the failing action itself. Requests are never retried after submission. After an uncertain timeout or disconnect, inspect current state before deciding what to do; never blindly replay a send, move or submission. Keep batches short enough for the90-second response deadline; declared waits in a batch are capped at60 seconds total.

### Verified application behavior

- **Common file dialogs in these installed apps:** Xed Save As… is a `file chooser`, with an unnamed `text` filename entry and `push button` Save; Open Files is a `file chooser`, and its `show_location` widget or Ctrl+L reveals path input. Thunar Create New Folder is a `dialog`, with an unnamed `text` entry and `push button` Create; its Rename dialog similarly has a `text` entry and `push button` Rename. Find the current container, then scope the entry/button lookup to it; check current state before relying on these observed labels. Close a saved, unchanged editor buffer before moving its file, avoiding the editor's missing-original-file prompt.
- **Xed:** accessible editing works for composing text and unsent `.eml` drafts. Save/Open use native choosers. The chooser's observed `show_location` action is another way to reveal path input. Moving a file while its buffer remains open can trigger a save prompt; distinguish that original buffer from the reopened final document before acting.
- **Thunar:** this installed version's icon view did not expose usable file items. Switch to detailed view with `ctrl+2` when semantic file selection is required; actual table cells expose filenames and actions. Focus an observed filename cell and use Return to open it; the cell action named `activate` returned success without navigating in the follow-up trial. Do not treat an empty accessibility result as an empty directory. Use `select` on the observed filename before cut/copy/rename; focusing a filename does not select it. After cutting files, navigate using keys or native field editing: `paste` replaces the clipboard, destroying the file-cut payload. When an already observed folder is selected, Return opens it. Use shortcuts and observed file controls to move and rename.
- **GNOME Calculator:** use the frame for paste/keyboard entry; the displayed result appears in labels/history, and the text entry can become empty after evaluation. Inspect both text and labels. Select all before replacing a previous expression.

These are observed behaviors of the installed apps, not promises for every toolkit/version. Visual fallback remains appropriate when content is missing.

## Signal desktop through its embedded renderer

```bash
agent-desktop browser "$W" --app signal --start
agent-desktop browser "$W" --app signal '[{"op":"inspect","query":"heading|button|textbox|link","limit":40}]'
```

This launches the installed Signal executable with a dedicated private profile and loopback debugging. It uses the same structured operations as [browser-control.md](browser-control.md), with a separate pinned tab/profile. Use `--app signal` on each call. An owner Signal window/profile is never attached. Startup waits for a usable debugging page; accessible text waits tolerate navigation races without replaying input.

The tested private Signal profile was unlinked: its onboarding heading and link were readable without screenshots. Sending through a linked Signal account was **not** tested. Do not claim delivery from typing text or successful input. Verify the intended recipient and actual sent state when sending is authorized.

## Telegram integration

For owner-authorized Telegram communication on this workstation, read `~/.local/share/agent-telegram/README.md` and use the shared `agent-telegram` transport. Register a unique task name; send with that name; read/wait on only that inbox and acknowledge handled IDs. The existing phone supervisor owns polling. Do not create another gateway, call getUpdates, pair a third party or handle the general inbox.

Use API acceptance/message IDs to report a send as accepted; recipient delivery/read confirmation is a separate fact. On timeout, do not blindly resend: the message may already have been sent. A real owner-self test measured36.5 seconds for the first send and1.08 seconds for a second send; the cause of that variability was not isolated. Do not promise screenshot-free messaging is instantaneous.

## Benchmarking and cleanup

Measure launch through a verified persisted outcome, including controller round trips and recovery. Report app launches, helper time, tool calls, screenshots and errors separately. A prepared macro measures repeated known work; a fresh controller discovering app state measures a different workload. Use real apps and preserve failed/recovered trials in the evidence.

The real cross-app trial used Calculator → Xed → Thunar → Xed to calculate, compose an unsent draft, create a folder, move/rename the draft and reopen/read it. The visual run took127.3 seconds with25 screenshots; the first native run took88.8 seconds with0 screenshots and2 recovered errors. A fresh follow-up took115.5 seconds with0 screenshots and two recovered waits; it was slower than the first native run. These are single sequential runs, not a general performance guarantee. A separate unchanged-state observation test measured a warm helper median of186.6 ms before per-observation bus ownership caching and124.4 ms afterward; that is helper time, not another whole-task speed result. No native mail client was installed; writing a draft file is not evidence of composing or sending through a mail client.

Close only the workspace you created: `agent-desktop close "$W"`.

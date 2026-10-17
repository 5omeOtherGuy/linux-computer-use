# Native batches and persistent sessions

`agent-desktop batch WS [JSON]` runs an array using the installed libxdo directly. JSON goes through no shell. Prefer stdin/heredocs or a language's JSON encoder for arbitrary text. A batch acknowledges X requests in order; asynchronous application work still needs a suitable observation or wait.

Each object has `"action"` and these fields:

| Action | Fields (defaults in parentheses) |
|---|---|
| `click` | `x`, `y`, `button` (1), `count` (1), `interval` (40 ms between clicks) |
| `move` | `x`, `y` |
| `key` | `keys`: array, e.g. `["ctrl+a","BackSpace"]`; `delay` (0 ms) |
| `paste` | `text`; `key` (`ctrl+v`; terminals generally `ctrl+shift+v`) |
| `type` | `text`; `delay` (0 ms ASCII; Unicode minimum 80 ms) |
| `scroll` | `direction`: up/down/left/right; `amount` (3), optional `x` and `y`; `interval` (0 ms) |
| `drag` | `x1`,`y1`,`x2`,`y2`; `button` (1), `duration` (0.12 seconds), `steps` (8) |
| `wait` | `seconds` |
| `wait_window` | Exactly one of `name`, `class`, `pid`; `timeout` (30 seconds) |
| `focus` | Exactly one of `window` (ID string), `name`, `class`, `pid` |
| `wait_stable` | Optional `region`: `X,Y,W,H`; `timeout` (5 seconds), `interval` (0.05 seconds), `samples` (2 matching comparisons after an initial sample) |
| `screenshot` | Optional `output`, `region`, `scale`, `window`; adds its path to `screenshots` |

Except where stated as milliseconds, time fields use seconds. Input modifiers are cleared and restored. Paste uses the installed xclip selection owner, which continues servicing asynchronous browser requests. A short 50 ms allowance lets the client insert text before a following paste replaces it. Removing clipboard ownership immediately after the first transfer failed a real Chromium form check, so keep this compatibility allowance. Clipboard changes are local to the workspace. Physical typing is a fallback for applications that reject paste, not the bulk-text default.

The automatic final screenshot has a short repaint allowance; it does not guarantee completion of navigation. A final explicit `screenshot` action suppresses the extra automatic image. `--no-screenshot` suppresses only the automatic image, not explicit ones.

## Keep the runtime between calls

For repeated interactions, `agent-desktop session WS` accepts one JSON array per line and emits one JSON response per line. It retains imports and its native X connection. It starts with `{"ready":true,"workspace":"WS"}`. EOF exits only the runner; still close the workspace when the task ends.

A request may also be `{"actions":[...],"screenshot":false}`. Errors become `{"error":"...","code":N}`; the session remains available. Never replay a partially completed request automatically: the failing action itself may already have sent input.

From a Python orchestration script (start once, reuse `p`):

```python
import json, subprocess
p = subprocess.Popen(["agent-desktop", "session", workspace],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     text=True, bufsize=1)
assert json.loads(p.stdout.readline())["ready"]
def act(actions, screenshot=True):
    p.stdin.write(json.dumps({"actions": actions, "screenshot": screenshot}) + "\n")
    p.stdin.flush()
    return json.loads(p.stdout.readline())
# Supply known coordinates or shortcuts, then open result["screenshots"] images.
result = act([{"action":"key", "keys":["ctrl+l"]}])
p.stdin.close()
p.wait(timeout=5)
```

Use a harness's existing persistent code runtime or writable subprocess session if available; do not build another daemon just to keep this process alive. For a one-off action sequence, use `batch`.

## Screenshot transport

When the harness can return images from code, execute → parse the JSON → load and emit the final image within one tool orchestration call. For Codex tools with `exec_command` and `view_image`, the pattern is:

```javascript
const r = await tools.exec_command({cmd: batchCommand, max_output_tokens: 1000});
if (r.exit_code !== 0) { text(r); } else {
  const result = JSON.parse(r.output);
  text({completed: result.completed, elapsed_ms: result.elapsed_ms});
  for (const path of result.screenshots) {
    const shot = await tools.view_image({path});
    image(shot.image_url);
  }
}
```

Only parse after the process finishes (handle a returned session ID with the harness's polling tool). Construct `batchCommand` with proper shell quoting or pass JSON through a literal heredoc; `JSON.stringify()` is not shell escaping. Other harnesses should use their native image-return mechanism.

For UI work requiring actual clicks/keyboard input, keep that requirement. For other tasks, an existing authorized API, accessibility interface, application command or Playwright selector can avoid visual searches and offer stronger state checks. Choose it when available and appropriate; this desktop runner does not provide a browser DOM or accessibility tree.

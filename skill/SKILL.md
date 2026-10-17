---
name: agent-desktop
description: Sessions read before opening, driving or screenshotting any GUI program (browser, GTK/Qt app, dialog, xterm) on this machine. Isolated throwaway X desktops that never touch the owner's screen; agent-desktop CLI, rules, recipes.
---

# agent-desktop

Each workspace is a private headless X server (Xvnc `:100`–`:199`) with openbox, its own D-Bus session bus, XDG config/data/cache dirs and runtime dir.
Programs in it cannot draw on, focus-steal from or send notifications, sound or single-instance hand-offs to the owner's desktop `:0`.
CLI: `agent-desktop` (source `~/projects/agent-desktop/bin/agent-desktop`); the owner watches workspaces from the panel menu.

<rules>
- Run GUI programs only through `agent-desktop run`; never set `DISPLAY=:0`, never run xdotool, wmctrl or xprop against `:0`.
- Never run `agent-desktop view`: it opens a window on the owner's desktop and belongs to the owner's panel menu.
- Create at most one workspace per task unless the brief says otherwise. Reuse it through the task and close **only the workspace you created** immediately afterward. The CLI enforces creator ownership using the harness PID/start time and Codex thread ID when available; `close --all` selects only that creator's workspaces. Prefer `close NAME` to keep cleanup scoped to the task. Without a recognized harness, ownership binds to the invoking parent process. `--owner-pid` controls automatic cleanup, not close authorization. Older workspaces without creator metadata are refused by `close` and retain automatic cleanup.
- Exit code 3 is a refusal (ownership, workspace cap or MemAvailable below 1.2 GiB): report it or wait; do not loop, and use `run --force` only for tiny programs such as xterm.
- Never type credentials into a workspace unless your brief authorizes that credential; screenshots and logs persist in `~/.cache/agent-desktop/ws/NAME/` until close.
- A workspace closes itself after 60 idle minutes (`--idle`) and when the creating claude/codex/p1 session exits (`--owner-pid auto`); do not rely on that for cleanup.
</rules>

<commands>
| command | effect |
|---|---|
| `agent-desktop new [--name N] [--label TEXT] [--geometry 1280x800] [--maximize] [--idle MIN]` | create; prints the name (stderr: display, files) |
| `agent-desktop run N [--wait] [--timeout S] -- PROG ARGS…` | start a program inside; prints `{"pid","log"}`; `--wait` prints its output and exits with its code |
| `agent-desktop wait N --name REGEX\|--class REGEX\|--pid P [--timeout 30]` | block until a visible window matches; prints its id |
| `agent-desktop screenshot N [-o FILE] [--window ID] [--region X,Y,W,H] [--scale F]` | PNG; prints its path |
| `agent-desktop click N X Y [--button 1\|2\|3] [--double] [--window ID]` | move and click (screen pixels) |
| `agent-desktop move N X Y` · `drag N X1 Y1 X2 Y2` · `scroll N up\|down\|left\|right [K] [--at X Y]` | pointer |
| `agent-desktop type N TEXT` · `type N --stdin` | type into the focused window (Unicode ok) |
| `agent-desktop key N KEY…` | xdotool key names: `Return`, `ctrl+l`, `alt+F4`, `Tab`, `Escape` |
| `agent-desktop windows N [--json]` · `focus N ID\|--name REGEX` | list / raise and focus windows |
| `agent-desktop list [--json]` · `info N` | live workspaces; details, pointer position |
| `agent-desktop env N` | shell exports for other X tools: `eval "$(agent-desktop env N)"; xclip -o` |
| `agent-desktop close N… \| --all` | close only workspaces created by this session; kill everything started in them, remove their files (`--keep-files` keeps shots and logs) |
Exit codes: 0 ok, 1 error, 2 usage or no such workspace, 3 refused, 4 timeout.
</commands>

<recipes>
```bash
W=$(agent-desktop new --label "$TASK")                      # one workspace for the task
agent-desktop run "$W" -- brave-browser --no-first-run https://example.org
agent-desktop wait "$W" --class brave --timeout 40
agent-desktop screenshot "$W"                              # then Read the PNG path it prints
agent-desktop click "$W" 640 80; agent-desktop key "$W" ctrl+a; agent-desktop type "$W" "query"; agent-desktop key "$W" Return
agent-desktop close "$W"
```
- Loop: screenshot → Read the PNG → act on pixel coordinates from that image → screenshot again to verify; with `--scale F`, divide coordinates by F before clicking.
- Browsers start with a fresh profile (private XDG dirs), so no owner logins.
- Never combine `new --share-home` with a browser the owner is running: the browser hands the URL to the owner's instance, which opens a window on `:0`.
- `--maximize` on `new` maximizes every window without decorations: simpler coordinates for single-app tasks.
- Program output and errors: the `log` path printed by `run`; server logs in `~/.cache/agent-desktop/ws/N/logs/`.
</recipes>

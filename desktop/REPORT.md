# agent-desktop — report

Isolated, throwaway GUI desktops for agents on this XFCE/X11 machine, with an owner view from the panel.
Built for the owner request of 2026-09-25.

## Decision

**One TigerVNC `Xvnc` server per workspace** (a variant of candidate (a)), each with openbox, a private D-Bus session bus, private XDG dirs and a private runtime dir.
The owner watches a workspace with `vncviewer` attached to the workspace's unix socket.

| candidate | verdict | reason |
|---|---|---|
| (a) Xvfb `:N` + WM, x11vnc on demand for viewing | rejected, close second | Same isolation as Xvnc and 2 MiB smaller (11.0 vs 13.5 MiB PSS). But viewing needs a second process per workspace (x11vnc, 9 MiB) with its own lifecycle. Xvnc is Xvfb with the VNC server built in: one process, always viewable, nothing to start or stop. |
| (a′) Xephyr `:N` | rejected | Xephyr is itself a window on `:0`, so every workspace would appear on the owner's desktop. |
| (b) extra XFCE workspaces on `:0` + forced placement | rejected | X11 has one pointer, one keyboard focus and one notification daemon per display. Agent clicks and typing would move the owner's mouse and steal focus. Automating it needs xdotool on `:0`, which the rules forbid. The Hyprland design works because the compositor isolates input per workspace; X11 cannot. |
| xpra, nested Wayland (cage/weston) | rejected | xpra is a heavier Python server (not installed). Wayland breaks the xdotool/wmctrl tooling and XFCE is X11. |

## Architecture

```
agent (claude/codex/p1) ── agent-desktop CLI ──┬─ Xvnc :1NN  (-auth cookie, -nolisten tcp, no TCP VNC port,
                                               │             VNC only on a 0600 unix socket, GLX off)
                                               ├─ openbox     (EWMH: wmctrl, activate, maximize)
                                               ├─ dbus-daemon (private session bus; D-Bus activated services start here)
                                               └─ apps        (env: DISPLAY=:1NN, private XAUTHORITY, bus, XDG_* dirs,
                                                               XDG_RUNTIME_DIR; PULSE_SERVER disabled; TMUX/WAYLAND/
                                                               SESSION_MANAGER unset; marker AGENT_DESKTOP_ID)
agent-desktop _reaper (one per user; exits 30 s after the last workspace)
    closes a workspace whose server died, which is idle > --idle (default 60 min),
    or whose creating session exited (--owner-pid auto = nearest claude/codex/p1/opencode/pi/amp ancestor)
owner: panel genmon item "🖥 N" → agent-desktop-menu (zenity) → vncviewer on :0 (view-only, or "Take control")
```

- State: `$XDG_RUNTIME_DIR/agent-desktop/ws/NAME/` (tmpfs, 0700): state, Xauthority, VNC socket, bus, runtime dir. Without `XDG_RUNTIME_DIR` it is `/tmp/agent-desktop-UID/agent-desktop`; both directories must belong to the user (anything else is refused) and are kept at 0700. The X cookie reaches `xauth` on stdin, never on a command line.
- Files: `~/.cache/agent-desktop/ws/NAME/` (0700): screenshots, logs, private XDG home. Removed on close unless `--keep-files`. The event log is `~/.cache/agent-desktop/events.log`.
- Cleanup kills every process of ours that carries the workspace marker in its environment: apps first, then openbox and dbus, then Xvnc. It also kills the owner viewers opened for that workspace.
- Guards:
  - at most `AGENT_DESKTOP_MAX` (3) workspaces;
  - `new` and `run` refuse with exit 3 when MemAvailable < 1.2 GiB (`AGENT_DESKTOP_MIN_AVAIL_MIB`=1229); `run --force` skips the check for tiny programs;
  - the CLI refuses any display outside `:100`–`:199`.

## Measurements (2026-09-25, this machine, 1280x800x24)

| item | PSS | RSS |
|---|---|---|
| Xvnc, GLX off (default) | 13.2 MiB | 21 MiB |
| Xvnc with 2 apps / while viewed | 17.4 / 21.7 MiB | 26 / 31 MiB |
| Xvfb, GLX off / GLX on (for comparison) | 11.0 / 57.1 MiB | 15 / 63 MiB |
| openbox | 8.7 MiB | 21 MiB |
| private dbus-daemon | 0.7 MiB | 4 MiB |
| **empty workspace total** | **≈ 22.5 MiB** | |
| reaper (one per user) | 10 MiB | |
| xterm / zenity (GTK3) | 6.8 / 19.1 MiB | 11 / 35 MiB |
| owner viewer (tigervnc vncviewer, only while open) | 16.8 MiB | 31 MiB |
| x11vnc (alternative design, per viewed workspace) | 9.0 MiB | |
| Brave on a data: page, 9 s after start (18 processes) | 67 MiB (PSS shares pages with the owner's Brave; real pages cost hundreds of MiB) | |

Latency (warm): `new` 0.6 s (3.9 s once under heavy swap), `close` 1.0 s, `screenshot` 0.42 s, `click` 0.29 s.
GLX is off by default because it costs 46 MiB per server; `new --glx` turns it on. Browsers render without it.

## Isolation evidence

- `tests/test_agent_desktop.py`, 8 end-to-end tests. They passed 3 consecutive runs (plus earlier runs); `:0`'s `_NET_CLIENT_LIST` was identical before and after every run (8 clients). `:0` is only read, with `xprop`.
  - lifecycle: an xterm is opened and gets focus, click, type (incl. `äö`), keys; the text reaches the program; screenshots (full and per-window) are checked; after close, the socket, display and processes are gone;
  - env isolation: DISPLAY, bus, XDG and runtime dirs are private; WAYLAND/SESSION_MANAGER/TMUX are unset; audio is off; a non-X `sleep` started inside is killed on close;
  - cap and RAM-guard refusals;
  - auto-close when the owner pid exits and after idle;
  - view: refused without `--owner`; with it, the viewer renders the other workspace;
  - panel: genmon output (escaped labels), and the menu, driven inside a workspace, opens the right viewer;
  - installer: `install-panel.sh --apply/--uninstall` run against a copy of the owner's panel config, served by a private xfconfd inside a workspace. A marker value proves the sandbox answered, and the owner's `plugin-ids` were identical afterwards.
- Brave (the owner's Brave running with 15 processes) opened in a workspace as a separate instance, rendered its page, and left `:0` unchanged. After close, no marked processes were left.
- Private D-Bus plus private XDG dirs mean these mechanisms stay inside the workspace: single-instance apps (xfce4-terminal, Thunar), notifications, xfconf/dconf writes and keyring prompts. Each workspace also has its own clipboard.

## Install

Packages were installed on 2026-09-25 with apt (simulated first, nothing removed): `tigervnc-standalone-server tigervnc-viewer openbox xterm xfce4-genmon-plugin x11vnc` plus 16 dependencies.
`x11vnc` was only for evaluation and the final design does not use it; removing it is the owner's call.
The design already relied on these installed tools: `xdotool wmctrl xauth dbus-daemon python3-pil zenity`.

1. Commands and panel item, run as the owner on `:0` from the repository root (dry run first):
   ```bash
   desktop/panel/install-panel.sh            # prints every change
   desktop/panel/install-panel.sh --apply    # links ~/.local/bin/{agent-desktop,agent-desktop-menu,agent-desktop-genmon},
                                                              # adds a Generic Monitor item left of the tray, restarts the panel
   desktop/panel/install-panel.sh --uninstall
   ```
   The script backs up the panel channel to `~/.config/agent-desktop/panel-backup-*.txt` before any panel change. It records the plugin id in `~/.config/agent-desktop/panel-plugin-id`.
   To add the item by hand instead: right-click the panel → Panel → Add New Items → Generic Monitor. Set Command `~/.local/bin/agent-desktop-genmon`, untick Label, and set the period to 3 s.
   To use a keyboard shortcut instead of the panel item: Settings → Keyboard → Application Shortcuts → `~/.local/bin/agent-desktop-menu`.
2. Skill (after review): link the bundled skill with `python3 scripts/install-desktop.py` from the repository root. Add a read trigger only if wanted: the skill's description names its trigger (any GUI program).
3. Agents need `agent-desktop` on PATH (step 1 links it), or they call it by path: `<checkout>/desktop/bin/agent-desktop`.

## Use

Agents: `skill/SKILL.md` (rules, commands, recipes). In short:
`W=$(agent-desktop new --label task)`, `agent-desktop run $W -- prog`, `wait`, `screenshot` then Read the PNG, `click/type/key`, `close $W`.

Owner:
- the panel shows nothing while no workspace runs, and `🖥 N` otherwise; the tooltip lists name, display and label;
- clicking it opens a list (name, label, display, age, idle, apps, RAM);
- **Watch** opens a view-only window; **Take control** lets the owner's mouse and keyboard act in the workspace;
- closing the viewer window leaves the workspace running; closing the workspace also closes its viewers.
- Command line: `agent-desktop list`, `agent-desktop view NAME --owner [--control]`, `agent-desktop close NAME`.

Tests: `cd <checkout>/desktop && python3 -m unittest discover -s tests -v` (~40–60 s, at most 2 workspaces at once; a test skips if the RAM guard refuses).

## Limits

- **Not a security sandbox.** Workspace programs run as the same user, with the same home directory, files, network and credentials on disk. Isolation is about the display, input, session bus and app config, not about access. A program may still read `~/.ssh` or write outside its XDG dirs.
- Non-XDG dotfiles are shared: apps that use `~/.foo` directly share the owner's state.
- `--share-home` gives apps the owner's configs. A browser then hands off to the owner's running instance, which opens on `:0`, so the skill forbids that combination.
- Cleanup finds processes by an environment marker. A program that clears its environment and is not an X client can survive close. X clients die with their server anyway.
- No GPU: rendering is software only. GLX is off unless `--glx` is given, and there is no Vulkan. Video and WebGL-heavy pages are slow.
- Only X11 apps work; Wayland-only apps cannot start (`GDK_BACKEND=x11` and `QT_QPA_PLATFORM=xcb` are forced).
- The keymap is US. Characters outside the keymap are typed through xdotool keycode remapping at ≥ 80 ms per character.
- Screenshots do not include the mouse cursor.
- Idle close defaults to 60 min (`--idle 0` disables). Owner auto-close follows the nearest agent-harness ancestor. A workspace created from a plain shell has no owner and relies on idle close.
- The RAM guard can refuse `new` on this machine: MemAvailable often sits at 1.0–1.5 GiB. The cap and threshold are env-tunable; raising them risks the owner's desktop.
- The genmon command runs every 3 s, as a POSIX sh script without X queries.

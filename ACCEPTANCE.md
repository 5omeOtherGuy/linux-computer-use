# Installed desktop capability acceptance — September 6, 2026

Installed from a local checkout and registered in Codex as `linux_desktop`, using `scripts/launch`. Codex's app server reported the server connected and exposed all five tools: `desktop_status`, `desktop_observe`, `desktop_exec`, `desktop_cancel`, `desktop_reset`.

The test installation used the owner's explicit authorization, represented by `mcp_servers.linux_desktop.default_tools_approval_mode = "approve"`. Other server policies and the global approval setting were unchanged.

## Fresh Codex session: passed

An actual ephemeral Codex CLI 0.153.4 session, using the installed configuration and model `gpt-6-astra`, completed the task through MCP tools. It did not use shell commands to manipulate the app. Its calls were:

1. `desktop_status`: discovered the dedicated acceptance window.
2. `desktop_observe`: inspected its controls and screenshot.
3. `desktop_exec`: entered `Fresh Codex native desktop acceptance — Grüße 391`, checked **Enable option**, clicked **Save message**, waited for **Saved successfully**, and observed the result.
4. `desktop_exec`: reused the same window variable, observed the window, and printed the exact Message text and checked state.

The separate fixture process independently recorded the exact saved string and `checked: true`; all four tool calls completed and Codex exited successfully. The screenshot was visually inspected. This verifies discovery, actual app control, observation and persistent execution through the real Codex client, beyond testing the MCP server alone.

Public evidence contains only the synthetic fixture and its results:

- [Independent result](docs/evidence/codex-acceptance-result.json)
- [Final screenshot](docs/demo.png)
- [Codex report](docs/evidence/codex-acceptance-final.txt)

Raw tool-call transcripts remain local and are excluded from Git.

Reproduce deliberately with `.venv/bin/python tests/acceptance_codex.py`; it uses normal model quota and briefly operates its own visible test app.

## Desktop and protocol coverage

Original XFCE acceptance run: `.venv/bin/python -m pytest -q` — **10 passed in 18.27 seconds**. This is test-suite runtime, not a comparative performance benchmark.

An additional release check under Xvfb/Openbox did **not** reproduce that clean result. The latest isolated run had **6 passed, 3 failed, 1 skipped**: Unicode/control lookup, drag verification and a worker exit during global input/diff testing failed. The systemd-login test was intentionally skipped in the virtual session. An isolated Unicode retry passed after using the API's state wait, but the full run still failed; that retry is not evidence of a complete fix. The experimental reproduction script is retained for contributors. No headless reliability claim or green CI badge is made. Further investigation was deliberately left outside this bounded publication review.

The live regression suite exercises GTK3 controls, mouse dragging on a custom canvas, scrolling, menus, a genuine file chooser, exact Unicode keyboard entry, accessible text selection, stable element IDs/diffs, global desktop input, stale/ambiguous/disabled targets, moved-window rejection, timeout input recovery, independent cancellation and competing-client exclusion. The Unicode typing case repeats ten times to detect keyboard-mapping races. Protocol tests verify tool discovery, image output, Python variable persistence/reset and startup with Codex's minimal environment.

Separately, the installed **GNOME Calculator (GTK4)** was launched and operated with named button actions to calculate `17 × 23`; its accessible result field and screenshot both showed **391**. This exposed and resolved a GTK4 ancestry compatibility issue. The optional bundled Sky pointer route was also exercised on this machine.

The session tested is **X11/XFCE**, display `:0`, with a 1600 × 900 screen. Native Wayland, mixed-DPI/multiple-monitor configurations, secure prompts, all application toolkits, and ChatGPT's proprietary takeover UI have not been validated or implemented. Core local desktop interaction is operational; full cross-platform product parity is not claimed.

## Integration defects resolved during acceptance

- Recover the standard user runtime directory before requesting the graphical environment; Codex removes variables that were present in direct shell tests.
- Set the server-specific tool authorization explicitly, since `approval_policy = "never"` alone does not authorize write tools.
- Pace temporary Unicode keyboard-map changes so the app can consume mapping notifications and key events.
- Accept live GTK4 ancestry even when an intermediate container reports an index of `-1`.
- Detach launched app stdio so GUI processes cannot keep the launcher's output pipes open.

These fixes are included in the installed runtime. No claim of measured speedup over ChatGPT is made. Persistent connections, batched actions, in-memory images and accessibility diffs remove identified sources of overhead.

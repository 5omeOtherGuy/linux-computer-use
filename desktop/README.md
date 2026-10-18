# agent-desktop

This is the desktop-management component of [Linux CUA](../README.md). Run the commands in this document from `desktop/`. Install command and skill links from the repository root with `python3 scripts/install-desktop.py`.

Isolated, throwaway X11 desktops for agents (Xvnc + openbox per workspace) with an XFCE panel menu for the owner to watch them.

- Decision, measurements, install, limits: [REPORT.md](REPORT.md)
- Agent instructions: [skill/SKILL.md](skill/SKILL.md)
- CLI: `bin/agent-desktop --help`; tests: `python3 -m unittest discover -s tests -v`

`close NAME` checks the recorded creator against the calling agent's harness PID
and process start time (and Codex thread ID when available). `close --all` closes
only that session's workspaces. Outside a recognized harness, ownership binds to
the invoking parent process, such as an interactive shell. `new --owner-pid`
controls automatic cleanup independently and does not grant permission to close.
Foreign workspaces and older workspaces without a recorded creator are refused
with exit 3; automatic idle, exited-server and exited-owner cleanup still applies.
This CLI guard prevents accidental cross-session closes; processes running as the
same Unix user can still modify the program or workspace state.

Ownership regression checks (no GUI):
`python3 -m unittest discover -s tests -p test_ownership.py -v`.

Screenshot-free web control: `agent-desktop browser WS --start --url URL` launches a dedicated private browser; `browser WS` accepts JSON operations to inspect accessible controls, fill forms, click observed references, read result rows and wait for relevant state. See [the browser reference](skill/references/browser-control.md). Python `websockets` and a Chromium browser are required.

Native desktop control: create with `new --a11y`, launch apps with `run`, then `native WS` reads and edits accessible controls without screenshots. Its persistent helper checks private workspace ownership and uses generation-scoped references. See [native control and assistant workflows](skill/references/native-control.md). Signal desktop also supports `browser WS --app signal` in its own disposable profile.

Fast input: `batch WS` reads a JSON action array from stdin; `session WS` keeps the native runtime open over JSON lines. Both return final screenshot paths by default. See [the batch/session reference](skill/references/fast-input.md).

Native correctness checks (one disposable desktop; no owner-display queries):

```bash
python3 -m unittest discover -s tests -p test_fast.py -v
```

For reproducible timing, start `tests/fast_fixture.py STATE_JSON` in a maximized private workspace, then run `python3 tests/benchmark_fast.py --workspace NAME --state STATE_JSON --baseline /absolute/path/to/archived-agent-desktop --output results.json`. The baseline is a saved version of the old CLI; benchmark results must retain input-correctness counts and distinguish local execution from model/tool latency.

The installed skill uses symlinks to `skill/SKILL.md` and `skill/references` so this checkout is the single source. The native input modules beside the CLI are required when relocating the executable.

Real workload measurement (2026-10-15): fresh GPT-6.1 Sol low controllers searched the actual GitHub issue form and verified the same three results. Visual trial: 66.2 seconds, 7 screenshots; initial structured trial: 28.4 seconds, 0 screenshots; improved bridge trial: 21.3 seconds, 0 screenshots. Timing spans first observation through the written verified result, including model/tool overhead. One task, sequential trials and a warm shared environment are insufficient for a general speed guarantee.

Native bridge checks (no display or app input): `python3 -m unittest discover -s tests -p test_native.py -v`. Real Calculator/Xed/Thunar workflow trials include app launch, arithmetic, an unsent email draft, folder creation, move/rename and reopening for verification. Visual: 127.3 seconds / 25 screenshots. Native: 88.8 seconds / 0 screenshots; follow-up: 115.5 seconds / 0 screenshots with two recovered waits. These single sequential runs show variability, not a general guarantee. The final ownership lookup optimization separately reduced median helper observation time from 186.6 to 124.4 ms on unchanged app state.

Further measured optimization (2026-10-16): request-local `find` bindings resolve unique current controls and feed later actions without another model turn. Browser result contexts return full row links/times; native `select` verifies file selection, including the GTK Table route. The original native workflow completed in 38.33 seconds versus its strongest prior 88.84-second baseline; a changed-input transfer trial took 41.67 seconds. The final browser form workflow completed in 8.00 seconds versus 21.28 seconds. Earlier failed/slower trials were retained on the measuring machine; these are per-run results, not a universal guarantee.

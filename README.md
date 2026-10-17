# agent-desktop

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

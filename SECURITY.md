# Execution boundary

This is a local desktop automation tool for trusted MCP clients. `desktop_exec`
runs persistent Python with the same OS privileges as the server process. It is
not a sandbox, and desktop permissions are not limited to the selected window.
X11 also grants broad access to the connected desktop. Do not expose the stdio
server through an unauthenticated network bridge.

The normal API validates observed geometry and accessible element identity,
serializes batches across clients, tracks synthetic input ownership, and releases
keys/buttons and temporary mappings after interruption. These checks reduce
accidental mis-targeting and stuck input; arbitrary Python can bypass them.
Observe the result after a timeout before retrying: earlier actions can already
have completed. Use `desktop_cancel` or `desktop_reset` to stop/reset the worker.
There is no global emergency-stop hotkey or takeover overlay in this release.

The MCP host remains responsible for user authorization, handling untrusted app
content, and approval before actions outside the user's requested scope. The
installer's `--allow-desktop-control` flag grants this server tool-level approval;
omit it if you want the host to prompt. It does not change other server policies.

Screenshots and accessibility observations may include private information and
are returned to the MCP host/model. Avoid sharing raw transcripts. Tests and the
manual Codex acceptance script write local artifacts that are excluded from Git.
The public demo image contains only the project's synthetic fixture app.

Report vulnerabilities through GitHub's private vulnerability reporting on this
repository when available. Do not put credentials, private screenshots, or
personal desktop transcripts in public issues.

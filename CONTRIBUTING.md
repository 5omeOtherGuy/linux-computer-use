# Contributing

Changes that improve the supported X11 path, reproduce an app incompatibility,
or add a separately tested backend are welcome. Keep capability reporting honest:
detecting XWayland is not proof of native Wayland control.

Install with `./scripts/install.py` to avoid changing your MCP configuration.
Runtime dependencies use system Python GI/AT-SPI/Pillow plus the pinned packages
in `requirements.lock`. Use `uv pip compile pyproject.toml -o requirements.lock`
when changing Python dependencies.

Tests need a graphical display. This **experimental reproduction recipe currently has known failures** under Xvfb/Openbox (see [ACCEPTANCE.md](ACCEPTANCE.md)); it is not a passing CI gate. On Ubuntu 24.04:

```sh
sudo apt install xvfb openbox dbus-x11 gir1.2-gtk-3.0 python3-pytest
xvfb-run -a -s '-screen 0 1600x900x24' dbus-run-session -- bash scripts/ci-desktop.sh
```

That run skips the test which specifically needs a real systemd graphical login.
Run `.venv/bin/python -m pytest -q` in an X11 login to include it; those tests
briefly move focus and inject input into their dedicated fixture window. Avoid
running a separate demo fixture with the same title at the same time.

`tests/acceptance_codex.py` is an optional manual test that starts a real Codex
session and consumes normal model quota. It is not run in CI and does not ship
account credentials. Do not commit its raw transcripts.

Include the desktop/session, app/toolkit version, reproduction and validation
with bug reports or pull requests. The current test coverage is described in
[ACCEPTANCE.md](ACCEPTANCE.md). Contributions are licensed under the project's
[MIT license](LICENSE).

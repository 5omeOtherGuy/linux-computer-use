#!/usr/bin/env bash
# Run inside: xvfb-run -a -s '-screen 0 1600x900x24' dbus-run-session -- bash scripts/ci-desktop.sh
set -euo pipefail
export XDG_SESSION_TYPE=x11
export NO_AT_BRIDGE=0
export LCU_TEST_HEADLESS=1
export GIO_USE_VFS=local
export GSETTINGS_BACKEND=memory
openbox > /dev/null 2>&1 &
lcu_wm_pid=$!
trap 'kill "$lcu_wm_pid" 2>/dev/null || true' EXIT
# Wait for the window manager's EWMH registration, not an arbitrary startup sleep.
.venv/bin/python - <<'PY'
import time
from Xlib import display
c = display.Display()
root = c.screen().root
atom = c.intern_atom('_NET_SUPPORTING_WM_CHECK')
deadline = time.monotonic() + 5
while root.get_full_property(atom, 0) is None:
    if time.monotonic() > deadline:
        raise RuntimeError('Openbox did not initialize')
    time.sleep(.02)
c.close()
PY
.venv/bin/python -m pytest -q "$@"

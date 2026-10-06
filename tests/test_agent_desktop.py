"""End-to-end tests for agent-desktop.

They start real Xvnc servers (at most two at once) under a private runtime dir
and never send input to the owner's display; :0 is only read with xprop.
Run: python3 -m unittest discover -s tests -v
"""
import json
import os
import re
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / "bin" / "agent-desktop")
OWNER_DISPLAY = os.environ.get("AGENT_DESKTOP_TEST_OWNER_DISPLAY", ":0")
RUNTIME = Path(f"/run/user/{os.getuid()}/adt-{os.getpid()}")
CACHE = Path(tempfile.mkdtemp(prefix="adt-cache-"))


def env(**extra):
    e = dict(os.environ)
    e.update({"AGENT_DESKTOP_RUNTIME": str(RUNTIME), "AGENT_DESKTOP_CACHE": str(CACHE)})
    e.update({k: str(v) for k, v in extra.items()})
    return e


def ad(*args, check=True, extra=None, input=None, timeout=60):
    r = subprocess.run([CLI, *args], env=env(**(extra or {})), capture_output=True, text=True,
                       input=input, timeout=timeout)
    if check and r.returncode != 0:
        raise AssertionError(f"agent-desktop {' '.join(args)} -> {r.returncode}\n{r.stderr}")
    return r


def owner_clients():
    """Top-level client windows on the owner's display (read-only)."""
    r = subprocess.run(["xprop", "-display", OWNER_DISPLAY, "-root", "_NET_CLIENT_LIST"],
                       capture_output=True, text=True, timeout=10)
    return sorted(re.findall(r"0x[0-9a-f]+", r.stdout))


def owner_window_pids(wids):
    pids = {}
    for w in wids:
        r = subprocess.run(["xprop", "-display", OWNER_DISPLAY, "-id", w, "_NET_WM_PID", "WM_CLASS"],
                           capture_output=True, text=True, timeout=10)
        pids[w] = r.stdout.strip().replace("\n", " ")
    return pids


def png_size(path):
    data = Path(path).read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    return struct.unpack(">II", data[16:24])


def new_ws(tc, *args, extra=None):
    r = ad("new", *args, "--owner-pid", "none", "--json", check=False, extra=extra)
    if r.returncode == 3 and "MemAvailable" in r.stderr:
        tc.skipTest("RAM guard refused: " + r.stderr.strip())
    tc.assertEqual(r.returncode, 0, r.stderr)
    return json.loads(r.stdout)


def run_app(tc, ws, *cmd):
    r = ad("run", ws, "--", *cmd, check=False)
    if r.returncode == 3 and "MemAvailable" in r.stderr:
        tc.skipTest("RAM guard refused: " + r.stderr.strip())
    tc.assertEqual(r.returncode, 0, r.stderr)
    return json.loads(r.stdout)["pid"]


class AgentDesktopTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.owner_before = owner_clients()
        cls.tmp = Path(tempfile.mkdtemp(prefix="adt-"))

    @classmethod
    def tearDownClass(cls):
        ad("close", "--all", check=False)
        after = owner_clients()
        shutil.rmtree(cls.tmp, ignore_errors=True)
        shutil.rmtree(CACHE, ignore_errors=True)
        if RUNTIME.exists():
            # the reaper exits 30 s after the last workspace; stop it now
            pidf = RUNTIME / "reaper.pid"
            if pidf.exists():
                try:
                    os.kill(int(pidf.read_text()), signal.SIGTERM)
                except (OSError, ValueError):
                    pass
            shutil.rmtree(RUNTIME, ignore_errors=True)
        new = sorted(set(after) - set(cls.owner_before))
        gone = sorted(set(cls.owner_before) - set(after))
        if new:
            raise AssertionError(f"owner display {OWNER_DISPLAY} gained windows: "
                                 f"{owner_window_pids(new)}")
        print(f"\n[isolation] {OWNER_DISPLAY} clients before={len(cls.owner_before)} after={len(after)} "
              f"new={len(new)} gone={len(gone)}", file=sys.stderr)

    def tearDown(self):
        ad("close", "--all", check=False)

    def test_lifecycle_interaction_and_isolation(self):
        ws = new_ws(self, "--name", "t-life")
        self.assertRegex(ws["display"], r"^:1\d\d$")
        out = self.tmp / "typed.txt"
        before = owner_clients()
        pid = run_app(self, "t-life", "xterm", "-geometry", "80x20+20+20", "-e", "sh", "-c",
                      f"cat > {out}")
        wid = ad("wait", "t-life", "--pid", str(pid), "--timeout", "15").stdout.split()[0]
        wins = json.loads(ad("windows", "t-life", "--json").stdout)
        self.assertTrue(any(w["pid"] == pid and "xterm" in w["class"].lower() for w in wins), wins)
        # the app window exists only in the workspace
        self.assertEqual(before, owner_clients(), "owner display changed while the app was open")
        ad("focus", "t-life", wid)
        ad("click", "t-life", "200", "150")
        ad("type", "t-life", "hello agent 42 äö")
        ad("key", "t-life", "Return")
        shot = ad("screenshot", "t-life").stdout.strip()
        self.assertEqual(png_size(shot), (1280, 800))
        crop = ad("screenshot", "t-life", "--window", wid, "-o", str(self.tmp / "w.png")).stdout.strip()
        w, h = png_size(crop)
        self.assertTrue(100 < w < 1280 and 100 < h < 800, (w, h))
        from PIL import Image
        im = Image.open(shot).convert("L")
        self.assertGreater(im.crop((30, 60, 400, 200)).getextrema()[1], 200, "xterm not visible")
        ad("key", "t-life", "ctrl+d")
        deadline = time.time() + 5
        while time.time() < deadline and not (out.exists() and out.read_text()):
            time.sleep(0.1)
        self.assertEqual(out.read_text(), "hello agent 42 äö\n")
        info = json.loads(ad("info", "t-life").stdout)
        self.assertEqual(info["mouse"]["X"], "200")
        sock = Path(info["vnc_socket"])
        self.assertEqual(sock.stat().st_mode & 0o777, 0o600)
        disp = int(ws["display"][1:])
        ad("close", "t-life")
        self.assertFalse(os.path.exists(f"/tmp/.X11-unix/X{disp}"))
        self.assertFalse(sock.exists())
        self.assertEqual(json.loads(ad("list", "--json").stdout), [])

    def test_env_is_isolated(self):
        new_ws(self, "--name", "t-env")
        r = ad("run", "t-env", "--wait", "--force", "--", "sh", "-c",
               'echo "$DISPLAY|$DBUS_SESSION_BUS_ADDRESS|$XDG_CONFIG_HOME|$XDG_RUNTIME_DIR|'
               '${WAYLAND_DISPLAY-unset}|${SESSION_MANAGER-unset}|${TMUX-unset}|$PULSE_SERVER|$DISABLE_AUTOUPDATER"')
        d, bus, cfg, rt, wl, sm, tmux, pulse, noupd = r.stdout.strip().split("|")
        self.assertEqual(noupd, "1")
        self.assertRegex(d, r"^:1\d\d$")
        self.assertIn(str(RUNTIME), bus)
        self.assertIn(str(CACHE), cfg)
        self.assertIn(str(RUNTIME), rt)
        self.assertEqual((wl, sm, tmux), ("unset", "unset", "unset"))
        self.assertIn("nonexistent", pulse)
        # a program started in the workspace is killed by close, even without X
        r = ad("run", "t-env", "--force", "--", "sleep", "600")
        pid = json.loads(r.stdout)["pid"]
        ad("close", "t-env")
        self.assertFalse(Path(f"/proc/{pid}").exists() and
                         Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z")

    def test_cap(self):
        new_ws(self, "--name", "t-cap1", extra={"AGENT_DESKTOP_MAX": 1})
        r = ad("new", "--name", "t-cap2", "--owner-pid", "none", check=False,
               extra={"AGENT_DESKTOP_MAX": 1})
        self.assertEqual(r.returncode, 3, r.stderr)
        self.assertIn("cap 1", r.stderr)

    def test_ram_guard(self):
        r = ad("new", "--name", "t-ram", check=False, extra={"AGENT_DESKTOP_MIN_AVAIL_MIB": 10 ** 7})
        self.assertEqual(r.returncode, 3)
        self.assertIn("MemAvailable", r.stderr)
        self.assertEqual(json.loads(ad("list", "--json").stdout), [])

    def test_auto_cleanup_owner_and_idle(self):
        sleeper = subprocess.Popen(["sleep", "600"])
        r = ad("new", "--name", "t-own", "--owner-pid", str(sleeper.pid), "--json", check=False)
        if r.returncode == 3 and "MemAvailable" in r.stderr:
            sleeper.kill()
            self.skipTest(r.stderr)
        self.assertEqual(r.returncode, 0, r.stderr)
        new_ws(self, "--name", "t-idle", "--idle", "0.05")  # 3 s
        sleeper.kill()
        sleeper.wait()
        deadline = time.time() + 20
        names = None
        while time.time() < deadline:
            names = [w["name"] for w in json.loads(
                subprocess.run([CLI, "list", "--json"], env=env(), capture_output=True,
                               text=True).stdout)]
            if not names:
                break
            time.sleep(1)
        self.assertEqual(names, [])
        log = (CACHE / "events.log").read_text()
        self.assertIn("close t-own", log)
        self.assertRegex(log, r"close t-own .*reason=owner-exited")
        self.assertRegex(log, r"close t-idle .*reason=(idle|owner-exited)")

    def test_view(self):
        new_ws(self, "--name", "t-src")
        dst = new_ws(self, "--name", "t-dst")
        run_app(self, "t-src", "xclock", "-geometry", "200x200+50+50")
        ad("wait", "t-src", "--class", "xclock", "--timeout", "15")
        r = ad("view", "t-src", check=False)
        self.assertEqual(r.returncode, 2, "view without --owner must refuse")
        dst_auth = str(RUNTIME / "ws" / "t-dst" / "Xauthority")
        r = ad("view", "t-src", "--owner", "--display", dst["display"], extra={"XAUTHORITY": dst_auth})
        self.assertIn("viewer_pid", r.stdout)
        ad("wait", "t-dst", "--class", "vnc", "--timeout", "15")
        time.sleep(1.5)
        shot = ad("screenshot", "t-dst").stdout.strip()
        from PIL import Image
        im = Image.open(shot).convert("L")
        bright = sum(1 for v in im.getdata() if v > 200)
        self.assertGreater(bright, 20000, "viewer does not show the 200x200 clock face")

    def test_panel_genmon_and_menu(self):
        genmon = str(ROOT / "panel" / "agent-desktop-genmon")
        out = subprocess.run([genmon], env=env(), capture_output=True, text=True).stdout
        self.assertIn("<txt></txt>", out)
        new_ws(self, "--name", "t-a", "--label", "target <x>")
        new_ws(self, "--name", "t-menu", "--label", "host")  # stands in for the owner's desktop
        out = subprocess.run([genmon], env=env(), capture_output=True, text=True).stdout
        self.assertIn("🖥 2", out)
        self.assertIn("t-a  :1", out)
        self.assertIn("target &lt;x&gt;", out)
        self.assertIn("<txtclick>", out)
        run_app(self, "t-a", "xclock", "-geometry", "200x200+50+50")
        run_app(self, "t-menu", str(ROOT / "panel" / "agent-desktop-menu"))
        ad("wait", "t-menu", "--name", "^Agent workspaces$", "--timeout", "20")
        time.sleep(0.8)
        ad("key", "t-menu", "Down", "Up", "Return")  # select row 1 (t-a), Watch
        wid = ad("wait", "t-menu", "--class", "vnc", "--timeout", "15").stdout.split()[0]
        title = [w["title"] for w in json.loads(ad("windows", "t-menu", "--json").stdout)
                 if int(w["id"], 16) == int(wid)]
        self.assertTrue(title and "agent:t-a" in title[0], title)

    def test_panel_installer_in_sandbox(self):
        """install-panel.sh --apply/--uninstall against a copy of the owner's panel config,
        served by a private xfconfd inside a workspace; the owner's panel is only read."""
        src = Path.home() / ".config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"
        if not src.exists() or not shutil.which("xfce4-panel"):
            self.skipTest("no xfce4-panel config to copy")
        owner_ids = subprocess.run(["xfconf-query", "-c", "xfce4-panel", "-p", "/panels/panel-1/plugin-ids"],
                                   capture_output=True, text=True).stdout
        new_ws(self, "--name", "t-pan")
        wsenv = dict(os.environ)
        for line in ad("env", "t-pan").stdout.splitlines():
            parts = shlex.split(line)
            if parts[0] == "export":
                k, v = parts[1].split("=", 1)
                wsenv[k] = v
            elif parts[0] == "unset":
                for k in parts[1:]:
                    wsenv.pop(k, None)
        dst = Path(wsenv["XDG_CONFIG_HOME"]) / "xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"
        dst.parent.mkdir(parents=True)
        dst.write_text(re.sub(r'(<property name="size" type="uint" value=")\d+', r"\g<1>41",
                              src.read_text(), count=1))
        run_app(self, "t-pan", "xfce4-panel")
        ad("wait", "t-pan", "--class", "xfce4-panel", "--timeout", "20")

        def q(*args):
            return subprocess.run(["xfconf-query", "-c", "xfce4-panel", *args], env=wsenv,
                                  capture_output=True, text=True).stdout.split()
        # safety: the queries must reach the sandbox's xfconfd, never the owner's
        self.assertEqual(q("-p", "/panels/panel-1/size"), ["41"])
        before = q("-p", "/panels/panel-1/plugin-ids")[2:]
        bindir = self.tmp / "bin"
        inst = [str(ROOT / "panel" / "install-panel.sh"), "--no-packages", "--bin-dir", str(bindir)]
        r = subprocess.run(inst + ["--apply"], env=wsenv, capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        after = q("-p", "/panels/panel-1/plugin-ids")[2:]
        added = [i for i in after if i not in before]
        self.assertEqual(len(added), 1, (before, after))
        pid_ = added[0]
        self.assertEqual(q("-p", f"/plugins/plugin-{pid_}"), ["genmon"])
        rc = Path(wsenv["XDG_CONFIG_HOME"]) / f"xfce4/panel/genmon-{pid_}.rc"
        self.assertIn(f"Command={bindir}/agent-desktop-genmon", rc.read_text())
        self.assertTrue((bindir / "agent-desktop").resolve() == ROOT / "bin" / "agent-desktop")
        time.sleep(3)
        r = subprocess.run(inst + ["--uninstall"], env=wsenv, capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(q("-p", "/panels/panel-1/plugin-ids")[2:], before)
        self.assertFalse(rc.exists())
        self.assertEqual(owner_ids, subprocess.run(
            ["xfconf-query", "-c", "xfce4-panel", "-p", "/panels/panel-1/plugin-ids"],
            capture_output=True, text=True).stdout)


if __name__ == "__main__":
    unittest.main()

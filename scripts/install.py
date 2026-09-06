#!/usr/bin/python3
"""Reproducible user-level install; register only the linux_desktop MCP entry."""
import argparse
import asyncio
import os
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--register", action="store_true", help="Register this checkout in the current user's Codex configuration")
parser.add_argument("--allow-desktop-control", action="store_true", help="Authorize this server's desktop tools without per-call approval (requires --register)")
args = parser.parse_args()
if args.allow_desktop_control and not args.register:
    parser.error('--allow-desktop-control requires --register')
root = Path(__file__).resolve().parents[1]
system_python = Path("/usr/bin/python3")
probe = subprocess.run([str(system_python), "-c", "import gi, pyatspi; from PIL import ImageGrab; assert __import__('sys').version_info >= (3,12)"], capture_output=True)
if probe.returncode:
    sys.exit("System Python >=3.12 needs GI, pyatspi and Pillow. On Debian/Ubuntu install python3-gi python3-pyatspi python3-pil gir1.2-atspi-2.0. The live test suite also needs gir1.2-gtk-3.0 and python3-pytest.")
uv = shutil.which("uv")
if not uv:
    sys.exit("Install uv first: https://docs.astral.sh/uv/getting-started/installation/")
python = root / ".venv/bin/python"
if not python.exists():
    subprocess.run([uv, "venv", "--python", str(system_python), "--system-site-packages", str(root / ".venv")], check=True)
subprocess.run([uv, "pip", "install", "--python", str(python), "-r", str(root / "requirements.lock")], check=True)
subprocess.run([uv, "pip", "install", "--python", str(python), "--no-deps", "-e", str(root)], check=True)
launcher = root / "scripts/launch"
launcher.chmod(0o755)
if args.register:
    codex = shutil.which("codex")
    if not codex:
        sys.exit("Installed desktop runtime, but codex is not on PATH.")
    config = Path(os.environ.get("CODEX_HOME", str(Path.home()/".codex"))) / "config.toml"
    document = tomllib.loads(config.read_text()) if config.exists() else {}
    existing = document.get("mcp_servers", {}).get("linux_desktop")
    if existing and (existing.get("command") != str(launcher) or existing.get("args")):
        sys.exit("Another linux_desktop server is already registered. It was not replaced.")
    if not existing:
        subprocess.run([codex, "mcp", "add", "linux_desktop", "--", str(launcher)], check=True)
    if args.allow_desktop_control:
        from configure_codex import allow_desktop_control
        asyncio.run(allow_desktop_control(codex))
    print("Registered linux_desktop. Open a new Codex session to discover its five desktop tools.")
else:
    print("Installed. Use --register to add linux_desktop to Codex.")

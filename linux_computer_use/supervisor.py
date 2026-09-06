"""Own worker lifetime, cross-client desktop lease, deadlines and recovery."""

import asyncio
import fcntl
import hashlib
import json
import os
import signal
import sys
import tempfile
from collections import deque
from pathlib import Path

from .errors import DesktopError


class Supervisor:
    def __init__(self):
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/linux-computer-use-{os.getuid()}"))
        runtime.mkdir(mode=0o700, exist_ok=True)
        self.directory = Path(tempfile.mkdtemp(prefix="linux-computer-use-", dir=runtime))
        self.ownership = self.directory / "input.json"
        display_hash = hashlib.sha256(os.environ.get("DISPLAY", "none").encode()).hexdigest()[:12]
        self.lease_path = runtime / f"linux-computer-use-{display_hash}.lock"
        self.lease = os.open(self.lease_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        self.process = None
        self.stderr_task = None
        self.stderr_tail = deque(maxlen=12)
        self.busy = False
        self.sequence = 0
        self._stop_lock = asyncio.Lock()

    async def _stderr(self, process):
        while line := await process.stderr.readline():
            self.stderr_tail.append(line.decode(errors="replace")[-1500:])

    async def _start(self):
        if self.process and self.process.returncode is None:
            return
        environment = dict(os.environ, LINUX_COMPUTER_USE_OWNERSHIP=str(self.ownership))
        self.process = await asyncio.create_subprocess_exec(
            sys.executable, "-u", "-m", "linux_computer_use.worker", env=environment,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            start_new_session=True, pass_fds=(self.lease,), limit=32 * 1024 * 1024)
        self.stderr_task = asyncio.create_task(self._stderr(self.process))
        raw = await asyncio.wait_for(self.process.stdout.readline(), 20)
        if not raw:
            raise DesktopError("WORKER_START_FAILED", "".join(self.stderr_tail)[-2500:] or "No graphical session available")
        if not json.loads(raw).get("ready"):
            raise DesktopError("WORKER_START_FAILED", raw.decode()[:1000])

    async def request(self, method, params=None, timeout=30):
        if self.busy:
            raise DesktopError("BUSY", "A desktop request is running; cancel it or wait for its result")
        try:
            fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise DesktopError("DESKTOP_BUSY", "Another local client owns desktop input; retry after its batch") from error
        self.busy = True
        try:
            await self._start()
            self.sequence += 1
            message = {"id": self.sequence, "method": method, "params": params or {}}
            self.process.stdin.write((json.dumps(message) + "\n").encode())
            await self.process.stdin.drain()
            raw = await asyncio.wait_for(self.process.stdout.readline(), timeout)
            if not raw:
                raise DesktopError("WORKER_EXITED", "Worker exited during execution; state may be partially changed")
            result = json.loads(raw)
            if result.get("id") != self.sequence:
                raise DesktopError("PROTOCOL_ERROR", "Worker response did not match request")
            return result
        except BaseException:
            import anyio
            with anyio.CancelScope(shield=True):
                await self.stop()
            raise
        finally:
            self.busy = False
            fcntl.flock(self.lease, fcntl.LOCK_UN)

    async def stop(self):
        async with self._stop_lock:
            process = self.process
            self.process = None
            if process and process.returncode is None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(process.wait(), 1)
                except asyncio.TimeoutError:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    await process.wait()
            # Kill any owned input helper that outlived the main worker.
            if process:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if self.stderr_task:
                await asyncio.wait_for(self.stderr_task, 2)
                self.stderr_task = None
            if self.ownership.exists():
                helper = await asyncio.create_subprocess_exec(
                    sys.executable, "-m", "linux_computer_use.recover", str(self.ownership),
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
                try:
                    _, stderr = await asyncio.wait_for(helper.communicate(), 5)
                except asyncio.TimeoutError:
                    helper.kill()
                    await helper.wait()
                    raise DesktopError("INPUT_RECOVERY_FAILED", "Input recovery timed out")
                if helper.returncode:
                    raise DesktopError("INPUT_RECOVERY_FAILED", stderr.decode()[-1500:])
            return {"cancelled": process is not None, "input_released": True, "runtime_reset": True}

    async def close(self):
        await self.stop()
        os.close(self.lease)
        for path in (self.ownership, self.ownership.with_suffix(".tmp")):
            path.unlink(missing_ok=True)
        self.directory.rmdir()

"""Persistent Python execution worker. Protocol output is separate from user print()."""

import contextlib
import ctypes
import io
import json
import os
import signal
import sys
import traceback

from .desktop import Desktop


def main():
    original_stdout = sys.stdout
    output = []
    output_size = 0

    def emit(item):
        nonlocal output_size
        output_size += len(json.dumps(item).encode())
        if output_size > 24 * 1024 * 1024 or len(output) >= 200:
            raise ValueError("Output limit reached; use a smaller observation")
        output.append(item)

    class Writer(io.TextIOBase):
        def __init__(self):
            self.pending = ""

        def write(self, text):
            self.pending += text
            if len(self.pending) > 65536:
                raise ValueError("Text output line exceeds 64 KiB")
            while "\n" in self.pending:
                line, self.pending = self.pending.split("\n", 1)
                if line:
                    emit({"type": "text", "text": line})
            return len(text)

        def flush(self):
            if self.pending:
                emit({"type": "text", "text": self.pending})
                self.pending = ""

    def send(value):
        original_stdout.write(json.dumps(value, ensure_ascii=False) + "\n")
        original_stdout.flush()

    parent = os.getppid()
    ctypes.CDLL(None).prctl(1, signal.SIGTERM)  # PR_SET_PDEATHSIG
    if os.getppid() != parent:
        return
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.signal(signal.SIGHUP, lambda *_: sys.exit(0))
    desktop = Desktop(os.environ["LINUX_COMPUTER_USE_OWNERSHIP"], emit)
    namespace = {"__builtins__": __builtins__, "desktop": desktop, "display": desktop.display}
    send({"ready": True, "session": desktop.session})
    try:
        for line in sys.stdin:
            if len(line) > 1024 * 1024:
                raise ValueError("Worker request exceeds limit")
            request = json.loads(line)
            output = []
            output_size = 0
            desktop.actions = []
            is_error = False
            result = None
            writer = Writer()
            try:
                with contextlib.redirect_stdout(writer), contextlib.redirect_stderr(writer):
                    if request["method"] == "status":
                        result = desktop.status()
                        emit({"type": "text", "text": json.dumps(result)})
                    elif request["method"] == "observe":
                        result = desktop.observe(**request["params"])
                    elif request["method"] == "exec":
                        code = request["params"]["code"]
                        if not code.strip() or len(code.encode()) > 128 * 1024:
                            raise ValueError("Code must be nonempty and at most 128 KiB")
                        exec(compile(code, "<desktop_exec>", "exec"), namespace)
                    else:
                        raise ValueError("Unknown worker method")
            except (SystemExit, KeyboardInterrupt):
                raise
            except BaseException as error:
                is_error = True
                output.append({"type": "text", "text": json.dumps({"error": getattr(error, "code", type(error).__name__),
                               "message": str(error), "traceback": traceback.format_exc()[-3500:]})})
            finally:
                try:
                    writer.flush()
                finally:
                    desktop.x.release()
            state = {"session": desktop.session, "actions": desktop.actions,
                     "execution_completed": not is_error}
            if result is not None:
                state["observation"] = result
            if not output:
                output.append({"type": "text", "text": "Execution completed; no observation was requested."})
            send({"id": request["id"], "content": output, "structuredContent": state, "isError": is_error})
    finally:
        desktop.x.release()
        if desktop.a11y:
            desktop.a11y.stop()


if __name__ == "__main__":
    main()

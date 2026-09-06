"""Manual end-to-end acceptance: a fresh Codex client uses the installed MCP server.

Runs one bounded model interaction, incurs normal model usage, and opens only the
fixture app. Not collected by pytest. Logs are functional evidence, not benchmarks.
"""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(__file__).resolve().parents[1]
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
marker = "Fresh Codex native desktop acceptance — Grüße 391"
prompt = f"""Run this bounded desktop-tool integration acceptance test only. Do not develop code, use subagents, touch other apps, change configuration, or address any skills-context warnings. Use the installed linux_desktop MCP tools. First discover deferred tools with tool search (search for linux_desktop), or if the host exposes code-mode tool metadata, look up linux_desktop there. Do not conclude that the tools are absent without attempting discovery. Then call desktop_status to discover the window named Linux Computer Use Acceptance and desktop_observe to inspect its UI. In desktop_exec, use the server's persistent Python API to enter {marker!r} in the Message field, enable Enable option, click Save message, and verify Saved successfully. Observe the resulting window with a screenshot. In a second desktop_exec call verify that the window variable persists and print the Message control text and checkbox state. Finish with a concise success/failure report based on observed UI. These changes to this test app are authorized. Do not use shell, browser, or filesystem tools for the task."""

with tempfile.TemporaryDirectory(prefix="lcu-codex-acceptance-") as temporary:
    fixture = subprocess.Popen([sys.executable, str(root / "tests/fixture_app.py")],
        env=dict(os.environ, LCU_FIXTURE_DIR=temporary), stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, start_new_session=True)
    try:
        assert fixture.stdout.readline().strip() == "READY"
        with (artifacts / "codex-acceptance.jsonl").open("w") as log, (artifacts / "codex-acceptance.stderr").open("w") as errors:
            completed = subprocess.run(["codex", "exec", "--ephemeral", "--json", "--skip-git-repo-check",
                "-C", temporary, "-c", 'approval_policy="never"',
                "-o", str(artifacts / "codex-acceptance-final.txt"), prompt],
                stdin=subprocess.DEVNULL, stdout=log, stderr=errors, timeout=180)
        state = json.loads((Path(temporary)/"state.json").read_text())
        records = [json.loads(line) for line in (artifacts/"codex-acceptance.jsonl").read_text().splitlines() if line.startswith("{")]
        calls = [record["item"] for record in records if record.get("type")=="item.completed" and record.get("item",{}).get("type")=="mcp_tool_call"]
        evidence = dict(codex_exit=completed.returncode, fixture_state=state,
                        tool_calls=[dict(server=c.get("server"),tool=c.get("tool"),status=c.get("status")) for c in calls])
        (artifacts/"codex-acceptance-result.json").write_text(json.dumps(evidence, indent=2, ensure_ascii=False)+"\n")
        print(json.dumps(evidence, ensure_ascii=False))
        assert completed.returncode == 0
        assert state["saved"] == marker and state["checked"]
        assert sum(c.get("tool")=="desktop_exec" for c in calls) >= 2
        assert any(c.get("tool")=="desktop_observe" for c in calls)
    finally:
        fixture.terminate()
        fixture.wait(timeout=5)

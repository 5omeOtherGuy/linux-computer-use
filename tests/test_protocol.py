import asyncio
import json
import os
import sys
from pathlib import Path
import pytest

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


def test_mcp_discovery_and_persistent_execution():
    async def run():
        params = StdioServerParameters(command=sys.executable, args=["-m", "linux_computer_use.server"], env=dict(os.environ))
        async with stdio_client(params) as streams:
            async with ClientSession(*streams) as client:
                initialized = await client.initialize()
                assert "persistent Python" in initialized.instructions
                names = {tool.name for tool in (await client.list_tools()).tools}
                assert names == {"desktop_status", "desktop_observe", "desktop_exec", "desktop_cancel", "desktop_reset"}
                status = await client.call_tool("desktop_status", {})
                assert not status.isError
                assert status.structuredContent["observation"]["capabilities"]["x11_input"]
                first = await client.call_tool("desktop_exec", {"code": "persistent_test_value = 42"})
                assert not first.isError
                second = await client.call_tool("desktop_exec", {"code": "print(persistent_test_value)"})
                assert not second.isError
                assert any("42" in getattr(c, "text", "") for c in second.content)
                screenshot = await client.call_tool("desktop_observe", {"screenshot": True})
                assert not screenshot.isError
                assert any(c.type == "image" for c in screenshot.content)
                assert any(c.type == "text" for c in screenshot.content)
                await client.call_tool("desktop_reset", {})
                reset = await client.call_tool("desktop_exec", {"code": "print(persistent_test_value)"})
                assert reset.isError
    asyncio.run(run())


@pytest.mark.skipif(os.environ.get('LCU_TEST_HEADLESS') == '1', reason='Requires a real systemd graphical login; Xvfb supplies its display explicitly')
def test_launcher_with_codex_minimal_environment():
    async def run():
        launcher = Path(__file__).resolve().parents[1] / 'scripts/launch'
        params = StdioServerParameters(command=str(launcher), env={'HOME':os.environ['HOME'], 'PATH':'/usr/bin:/bin'})
        async with stdio_client(params) as streams:
            async with ClientSession(*streams) as client:
                await client.initialize()
                result = await client.call_tool('desktop_status', {})
                assert not result.isError
                assert result.structuredContent['observation']['capabilities']['x11_input']
    asyncio.run(run())

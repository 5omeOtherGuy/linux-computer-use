"""Write only this server's authorized policy through Codex's versioned config API."""
import asyncio
import json
from pathlib import Path


async def allow_desktop_control(codex):
    process = await asyncio.create_subprocess_exec(codex, 'app-server', '--stdio',
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL, limit=4*1024*1024)
    sequence = 0

    async def call(method, params):
        nonlocal sequence
        sequence += 1
        process.stdin.write((json.dumps(dict(id=sequence, method=method, params=params))+'\n').encode())
        await process.stdin.drain()
        async with asyncio.timeout(30):
            while line := await process.stdout.readline():
                response = json.loads(line)
                if response.get('id') == sequence:
                    if 'error' in response:
                        raise RuntimeError(response['error'])
                    return response['result']
        raise RuntimeError('Codex config connection closed')

    try:
        await call('initialize', {'clientInfo':{'name':'linux-desktop-installer','version':'0.1.0'}})
        config = await call('config/read', {'includeLayers':True})
        user = next(layer for layer in config['layers'] if layer['name']['type']=='user' and not layer['name'].get('profile'))
        existing = user['config'].get('mcp_servers',{}).get('linux_desktop',{})
        expected_command = str(Path(__file__).resolve().with_name('launch'))
        if existing.get('command') != expected_command:
            raise RuntimeError('Refusing to change another linux_desktop registration')
        result = await call('config/value/write', {
            'filePath':user['name']['file'], 'expectedVersion':user['version'],
            'keyPath':'mcp_servers.linux_desktop.default_tools_approval_mode',
            'mergeStrategy':'replace', 'value':'approve'})
        print('Authorized desktop tool use for linux_desktop only:', result.get('status','saved'))
    finally:
        process.terminate()
        await process.wait()

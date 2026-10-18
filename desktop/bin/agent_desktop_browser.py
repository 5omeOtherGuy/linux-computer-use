"""Structured controls for a browser launched in an isolated agent-desktop workspace."""
import asyncio
import json
import math
import re
import sys
import time
import urllib.request
from urllib.parse import urlsplit
from pathlib import Path
import websockets

class Browser:

    def __init__(self, profile):
        self.profile = Path(profile)

    async def __aenter__(self):
        active = (self.profile / 'DevToolsActivePort').read_text().splitlines()
        port = int(active[0])
        if not 0 < port < 65536:
            raise ValueError('Invalid private browser port')
        port = str(port)
        base = 'http://127.0.0.1:' + port
        version = json.load(urllib.request.urlopen(base + '/json/version', timeout=3))
        if version.get('webSocketDebuggerUrl') != 'ws://127.0.0.1:' + port + active[1]:
            raise ValueError('Browser identity differs from private profile; refusing attachment')
        pages = json.load(urllib.request.urlopen(base + '/json/list', timeout=3))
        target_file = self.profile / 'agent-desktop-target'
        if target_file.exists():
            ident = target_file.read_text().strip()
            page = next((p for p in pages if p['type'] == 'page' and p['id'] == ident), None)
            if page is None:
                raise ValueError('Selected tab closed; refusing to switch tabs silently')
        else:
            tabs = [p for p in pages if p['type'] == 'page']
            if len(tabs) != 1:
                raise ValueError('Expected one initial tab')
            page = tabs[0]
            target_file.write_text(page['id'])
        endpoint = page['webSocketDebuggerUrl']
        if not endpoint.startswith('ws://127.0.0.1:' + port + '/'):
            raise ValueError('Nonlocal browser endpoint')
        self.ws = await websockets.connect(endpoint, max_size=20 * 1024 * 1024)
        self.seq = 0
        return self

    async def __aexit__(self, *args):
        await self.ws.close()

    async def call(self, method, **params):
        self.seq += 1
        ident = self.seq
        await self.ws.send(json.dumps(dict(id=ident, method=method, params=params)))
        while True:
            try:
                row = json.loads(await asyncio.wait_for(self.ws.recv(), 15))
            except asyncio.TimeoutError:
                raise TimeoutError('CDP ' + method + ' response timed out; inspect current state before retrying input') from None
            if row.get('id') == ident:
                if 'error' in row:
                    raise RuntimeError(row['error'])
                return row.get('result', {})

    async def eval(self, expression):
        r = await self.call('Runtime.evaluate', expression=expression, returnByValue=True, awaitPromise=True)
        if 'exceptionDetails' in r:
            raise RuntimeError(r['exceptionDetails'].get('text', 'evaluation failed'))
        return r['result'].get('value')

    async def node(self, ref):
        if not ref.startswith('b') or not ref[1:].isdigit():
            raise ValueError('Use an observed bNNN reference')
        return (await self.call('DOM.resolveNode', backendNodeId=int(ref[1:])))['object']['objectId']

    async def element(self, ref, fn, args=None):
        oid = await self.node(ref)
        r = await self.call('Runtime.callFunctionOn', objectId=oid, functionDeclaration=fn, arguments=[{'value': x} for x in args or []], returnByValue=True)
        if 'exceptionDetails' in r:
            raise RuntimeError(r['exceptionDetails'].get('text', 'element operation failed'))
        return r['result'].get('value')

    async def press(self, key):
        name, code, vk, mods = parse_key(key)
        extra = {'text': name, 'unmodifiedText': name} if len(name) == 1 and not mods & 7 else {}
        if name.lower() == 'a' and mods == 2:
            extra['commands'] = ['selectAll']
        await self.call('Input.dispatchKeyEvent', type='keyDown', key=name, code=code, windowsVirtualKeyCode=vk, modifiers=mods, **extra)
        await self.call('Input.dispatchKeyEvent', type='keyUp', key=name, code=code, windowsVirtualKeyCode=vk, modifiers=mods)

    async def inspect(self, query='', limit=100, scope=None, main=False, contexts=False, role=None, lookup=False):
        meta = await self.eval('({url:location.href,title:document.title,ready:document.readyState})')
        tree = (await self.call('Accessibility.getFullAXTree'))['nodes']
        by_id = {n['nodeId']: n for n in tree}
        ordered = []
        seen = set()

        def visit(n):
            if n['nodeId'] in seen:
                return
            seen.add(n['nodeId'])
            ordered.append(n)
            for ident in n.get('childIds', []):
                if ident in by_id:
                    visit(by_id[ident])
        root = next((n for n in tree if str(n.get('backendDOMNodeId')) == str(scope)[1:]), None) if scope else tree[0] if tree else None
        if scope and root is None:
            raise ValueError('Scope reference is stale; inspect again')
        if main:
            root = next((n for n in tree if not n.get('ignored') and n.get('role', {}).get('value') == 'main'), root)
        if root:
            visit(root)
        tree = ordered
        rows = []
        terms = [x.strip().lower() for x in query.split('|') if x.strip()]
        for n in tree:
            if n.get('ignored'):
                continue
            node_role = n.get('role', {}).get('value', '')
            if role is not None and node_role != role:
                continue
            name = n.get('name', {}).get('value', '')
            value = n.get('value', {}).get('value', '')
            props = {p['name']: p.get('value', {}).get('value') for p in n.get('properties', []) if p['name'] in ('disabled', 'checked', 'selected', 'expanded', 'required', 'level', 'editable', 'focused')}
            if not lookup and (node_role in ('InlineTextBox', 'generic', 'none') or not (name or value or node_role in ('textbox', 'combobox', 'searchbox'))):
                continue
            if terms and (not any((t in (str(name) + ' ' + node_role + ' ' + str(value)).lower() for t in terms))):
                continue
            row = dict(ref='b' + str(n.get('backendDOMNodeId', 0)), role=node_role, name=str(name)[:400])
            if len(str(name)) > 400:
                row['name_truncated'] = True
            parent = by_id.get(n.get('parentId'), {})
            if parent.get('backendDOMNodeId'):
                row['parent'] = 'b' + str(parent['backendDOMNodeId'])
            if value:
                row['value'] = str(value)[:300]
            if props:
                row['state'] = props
            rows.append(row)
        if contexts:
            for row in rows[:limit]:
                row['context'] = await self.context(row['ref'], strict=False)
        return dict(**meta, total_matches=len(rows), nodes=rows[:limit], truncated=len(rows) > limit)

    async def context(self, ref, strict=True):
        return await self.element(ref, "function(strict){let target=this.nodeType===1?this:this.parentElement,e=target&&target.closest('li,tr,[role=row],[role=listitem]');if(!e){if(strict)throw Error('No list or table row ancestor; use read or scoped inspect');return null;}return {text:(e.innerText||'').slice(0,12000),links:[...e.querySelectorAll('a[href]')].slice(0,40).map(a=>({text:a.innerText,url:a.href})),times:[...e.querySelectorAll('relative-time,time')].map(t=>({text:t.innerText||(t.shadowRoot&&t.shadowRoot.textContent)||t.textContent||'',datetime:t.getAttribute('datetime'),label:t.getAttribute('aria-label')}))}}", [strict])

    async def run(self, a):
        op = a['op']
        if op == 'find':
            end = time.monotonic() + a.get('timeout', 3)
            while True:
                snapshot = await self.inspect(query=a.get('name', ''), limit=1000, scope=a.get('scope'), main=a.get('main', False), role=a['role'], lookup=True)
                if snapshot['truncated']:
                    raise ValueError('Incomplete browser lookup; narrow scope')
                matches = [row for row in snapshot['nodes'] if 'name' not in a or (row['name'] == a['name'] and not row.get('name_truncated'))]
                if len(matches) > 1:
                    raise ValueError('Ambiguous browser selector; use an exact name or scope')
                if matches:
                    return matches[0]
                if time.monotonic() >= end:
                    raise TimeoutError('Browser selector did not match; inspect current controls')
                await asyncio.sleep(0.05)
        if op == 'inspect':
            return await self.inspect(a.get('query', ''), a.get('limit', 100), a.get('scope'), a.get('main', False), a.get('contexts', False), a.get('role'))
        if op == 'wait_url':
            end = time.monotonic() + a.get('timeout', 10)
            while time.monotonic() < end:
                meta = await self.eval('({url:location.href,ready:document.readyState})')
                match = a['url'] in meta['url'] if a.get('contains') else meta['url'] == a['url']
                if match and meta['ready'] != 'loading':
                    return meta
                await asyncio.sleep(0.1)
            raise TimeoutError('URL condition not reached; last state: ' + json.dumps(meta))
        if op == 'goto':
            r = await self.call('Page.navigate', url=a['url'])
            if r.get('errorText'):
                raise RuntimeError('Navigation failed: ' + r['errorText'])
            if r.get('isDownload'):
                raise RuntimeError('Navigation started a download instead of a document')
            end = time.monotonic() + a.get('timeout', 12)
            meta = {}
            while time.monotonic() < end:
                try:
                    # Redirects retain the navigation loader ID; reject the previous ready document.
                    frame = (await self.call('Page.getFrameTree'))['frameTree']['frame']
                    if frame['id'] == r['frameId'] and (not r.get('loaderId') or frame.get('loaderId') == r['loaderId']):
                        meta = await self.eval('({url:location.href,ready:document.readyState})')
                        if meta['ready'] != 'loading':
                            return meta
                except RuntimeError as error:
                    if not any((text in str(error) for text in ('Execution context was destroyed', 'Cannot find context'))):
                        raise
                await asyncio.sleep(0.1)
            raise TimeoutError('Navigation did not finish; last state: ' + json.dumps(meta))
        if op == 'press':
            await self.call('Page.bringToFront')
            for key in a['keys']:
                await self.press(key)
            return {'pressed': a['keys']}
        if op == 'fill':
            info = await self.element(a['ref'], 'function(){return {tag:this.tagName,type:this.type,editable:this.isContentEditable,disabled:this.disabled,readonly:this.readOnly}}')
            if info.get('disabled') or info.get('readonly') or (info.get('tag') not in ('INPUT', 'TEXTAREA') and (not info.get('editable'))):
                raise ValueError('Target is not editable')
            await self.call('Page.bringToFront')
            await self.call('DOM.focus', backendNodeId=int(a['ref'][1:]))
            focused = await self.element(a['ref'], "function(){return this.matches(':focus')||this===document.activeElement||this.contains(document.activeElement)}")
            if not focused:
                raise RuntimeError('Editable target did not receive focus; no text inserted')
            await self.press('ctrl+a')
            await self.call('Input.insertText', text=a['text'])
            actual = await self.element(a['ref'], "function(){return this.isContentEditable?this.innerText:this.value}")
            if actual != a['text']:
                raise RuntimeError('Fill readback differs from requested text; inspect before continuing')
            return {'filled': a['ref']}
        if op == 'click':
            await self.call('Page.bringToFront')
            rect = await self.element(a['ref'], "function(){this.scrollIntoView({block:'center',inline:'center'});let r=this.getBoundingClientRect(),x=r.x+r.width/2,y=r.y+r.height/2,t=document.elementFromPoint(x,y);if(!r.width||!r.height||!(t===this||this.contains(t)))throw Error('Target hidden or obstructed');return {x,y}}")
            for t in ('mousePressed', 'mouseReleased'):
                await self.call('Input.dispatchMouseEvent', type=t, button='left', clickCount=1, **rect)
            return {'clicked': a['ref']}
        if op == 'context':
            return await self.context(a['ref'])
        if op == 'read':
            return await self.element(a['ref'], "function(){return {tag:this.tagName,text:(this.innerText||this.textContent||'').slice(0,16000),href:this.href||null,links:[...this.querySelectorAll('a[href]')].slice(0,40).map(a=>({text:a.innerText,url:a.href}))}}")
        if op == 'wait':
            end = time.monotonic() + a.get('timeout', 8)
            while time.monotonic() < end:
                try:
                    r = await self.inspect(a['query'], a.get('limit', 60), main=a.get('main', False), contexts=a.get('contexts', False), role=a.get('role'))
                    if r['nodes']:
                        return r
                except RuntimeError as error:
                    if not any(message in str(error) for message in ('Execution context was destroyed', 'Cannot find context', 'Inspected target navigated or closed')):
                        raise
                await asyncio.sleep(0.1)
            raise TimeoutError('No accessible text matched ' + a['query'])
        raise ValueError('Unknown op ' + op)

def parse_key(key):
    if not isinstance(key, str):
        raise ValueError('Key must be a string')
    aliases = {'Return': ('Enter', 'Enter', 13), 'Tab': ('Tab', 'Tab', 9), 'Escape': ('Escape', 'Escape', 27), 'BackSpace': ('Backspace', 'Backspace', 8), 'Home': ('Home', 'Home', 36), 'End': ('End', 'End', 35), 'Down': ('ArrowDown', 'ArrowDown', 40), 'Up': ('ArrowUp', 'ArrowUp', 38), 'Left': ('ArrowLeft', 'ArrowLeft', 37), 'Right': ('ArrowRight', 'ArrowRight', 39), 'space': (' ', 'Space', 32)}
    parts = key.split('+')
    plain = parts[-1]
    mods = 0
    for token in parts[:-1]:
        flag = {'ctrl': 2, 'shift': 8, 'alt': 1, 'meta': 4}.get(token.lower())
        if flag is None:
            raise ValueError('Unsupported modifier ' + token)
        mods |= flag
    if plain in aliases:
        name, code, vk = aliases[plain]
    elif len(plain) == 1 and plain.isascii() and plain.isalnum():
        name, code, vk = (plain, ('Key' if plain.isalpha() else 'Digit') + plain.upper(), ord(plain.upper()))
    else:
        raise ValueError('Unsupported key ' + plain)
    return (name, code, vk, mods)

def validate(actions):
    if not isinstance(actions, list) or len(actions) > 100:
        raise ValueError('Expected at most 100 browser operations')
    schemas = {'inspect': (set(), {'query', 'limit', 'scope', 'main', 'contexts', 'role'}), 'goto': ({'url'}, {'timeout'}), 'press': ({'keys'}, set()), 'fill': ({'ref', 'text'}, set()), 'click': ({'ref'}, set()), 'read': ({'ref'}, set()), 'context': ({'ref'}, set()), 'wait': ({'query'}, {'timeout', 'limit', 'main', 'contexts', 'role'}), 'wait_url': ({'url'}, {'timeout', 'contains'}), 'find': ({'role', 'as'}, {'name', 'scope', 'main', 'timeout'})}
    bindings = set()
    for a in actions:
        if not isinstance(a, dict) or not isinstance(a.get('op'), str) or a['op'] not in schemas:
            raise ValueError('Unknown browser operation')
        required, optional = schemas[a['op']]
        if required - set(a):
            raise ValueError('Missing fields for ' + a['op'] + ': ' + ','.join(sorted(required - set(a))))
        if set(a) - required - optional - {'op'}:
            raise ValueError('Unknown fields for ' + a['op'])
        for field in ('ref', 'scope'):
            if field in a:
                if isinstance(a[field], str) and a[field].startswith('$'):
                    if a[field][1:] not in bindings:
                        raise ValueError('Binding must be defined by an earlier find operation')
                elif not isinstance(a[field], str) or not re.fullmatch('b[1-9][0-9]{0,9}', a[field]) or int(a[field][1:]) > 2147483647:
                    raise ValueError('Invalid ' + field + ' reference')
        for field in ('query', 'text', 'url', 'role', 'name', 'as'):
            if field in a and (not isinstance(a[field], str) or '\x00' in a[field] or len(a[field]) > 100000):
                raise ValueError('Invalid ' + field + ' string')
        if a['op'] == 'find':
            if not re.fullmatch('[A-Za-z][A-Za-z0-9_]{0,39}', a['as']) or a['as'] in bindings:
                raise ValueError('find needs a unique simple binding name')
            if 'name' in a and len(a['name']) > 400:
                raise ValueError('Exact name lookup is limited to 400 characters')
            bindings.add(a['as'])
        if 'role' in a and not re.fullmatch('[A-Za-z][A-Za-z0-9]*', a['role']):
            raise ValueError('role must be an exact accessibility role')
        if a['op'] == 'wait' and (not a['query'].strip()):
            raise ValueError('Wait query must not be empty')
        if 'url' in a and not a['url']:
            raise ValueError('URL condition must not be empty')
        if a['op'] == 'goto':
            url = urlsplit(a['url'])
            if any(c.isspace() or ord(c) < 32 for c in a['url']) or not (url.scheme in ('http', 'https') and url.hostname or a['url'] == 'about:blank'):
                raise ValueError('Expected an absolute HTTP(S) URL or about:blank')
        if 'limit' in a and (type(a['limit']) is not int or not 1 <= a['limit'] <= 1000):
            raise ValueError('limit must be an integer from 1 to 1000')
        if 'timeout' in a:
            t = a['timeout']
            if type(t) not in (int, float) or not 0 < t <= 60 or (not math.isfinite(t)):
                raise ValueError('timeout must be finite and greater than 0, at most 60 seconds')
        for field in ('main', 'contains', 'contexts'):
            if field in a and type(a[field]) is not bool:
                raise ValueError(field + ' must be boolean')
        if a.get('main') and 'scope' in a:
            raise ValueError('Use main or scope, not both')
        if 'keys' in a:
            if not isinstance(a['keys'], list) or not 1 <= len(a['keys']) <= 100:
                raise ValueError('keys must contain 1 to 100 keys')
            for key in a['keys']:
                parse_key(key)
    return actions

async def execute(actions, profile, completed=None):
    # Reject the entire malformed batch before attaching or sending any input.
    # Runtime failures can still follow partial input; preserve completed results.
    validate(actions)
    if completed is None:
        completed = []
    bindings = {}
    async with Browser(profile) as b:
        for a in actions:
            item = dict(a)
            for field in ('ref', 'scope'):
                if field in item and item[field].startswith('$'):
                    item[field] = bindings[item[field][1:]]
            result = await b.run(item)
            completed.append(result)
            if item['op'] == 'find':
                bindings[item['as']] = result['ref']
    return completed

def run_cli(a, w, Fail, cmd_run):
    import argparse, shutil
    app = getattr(a, 'app', 'browser')
    profile = w.files / ('signal-control' if app == 'signal' else 'browser-control')
    w.touch()
    if w.state.get('share_home'):
        raise Fail('Browser controls require a private workspace')
    if a.start:
        if a.actions is not None:
            raise Fail('--start does not accept actions; send actions in a subsequent invocation')
        try:
            validate([{'op': 'goto', 'url': a.url if a.url is not None else 'about:blank'}])
        except ValueError as error:
            raise Fail(str(error))
        if (profile / 'DevToolsActivePort').exists():
            raise Fail('Browser already started; reuse browser operations without --start')
        program = shutil.which('signal-desktop') if app == 'signal' else (shutil.which('brave-browser') or shutil.which('chromium') or shutil.which('google-chrome'))
        if not program:
            raise Fail('Requested desktop application is not installed')
        args = [program, '--no-first-run', '--password-store=basic', '--disable-gpu', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', '--user-data-dir=' + str(profile)]
        if app == 'signal':
            if a.url:raise Fail('--url is supported only for browser startup')
            args += ['--force-renderer-accessibility']
        else:
            args.append(a.url or 'about:blank')
        cmd_run(argparse.Namespace(ws=w.name, cmd=args, force=False, cwd=None, wait=False))
        end = time.monotonic() + 15
        async def ready():
            async with Browser(profile):
                return True
        last_error = 'debugger endpoint not ready'
        while time.monotonic() < end:
            if (profile / 'DevToolsActivePort').exists():
                try:
                    asyncio.run(ready())
                    return
                except Exception as error:
                    last_error = str(error)
            time.sleep(0.1)
        raise Fail('Private application did not expose controls; inspect its run log: ' + last_error)
    if a.url is not None:
        raise Fail('--url requires --start')
    start = time.perf_counter()
    completed = []
    try:
        actions = json.loads(a.actions if a.actions is not None else sys.stdin.read())
        asyncio.run(execute(actions, profile, completed))
    except Exception as error:
        print(json.dumps(dict(completed=len(completed), results=completed, error=str(error), elapsed_ms=(time.perf_counter() - start) * 1000), ensure_ascii=False))
        raise Fail('Browser batch failed; inspect completed results before retrying')
    print(json.dumps(dict(completed=len(completed), results=completed, elapsed_ms=(time.perf_counter() - start) * 1000), ensure_ascii=False))

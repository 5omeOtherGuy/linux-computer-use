"""Persistent AT-SPI controls scoped to one private agent-desktop workspace."""
import argparse
import ctypes
import errno
import secrets
import stat
import struct
import json
import math
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time
MAX_REQUEST = 2 * 1024 * 1024
MAX_RESPONSE = 16 * 1024 * 1024
REF_PATTERN = 'n[0-9a-f]{12}_[1-9][0-9]{0,9}'
_KEYSYM = None

def key_chord(key):
    global _KEYSYM
    if not isinstance(key, str) or len(key) > 100 or (not re.fullmatch('[A-Za-z0-9_+]+', key)):
        raise ValueError('Invalid key chord')
    parts = key.split('+')
    aliases = {'ctrl': 'ctrl', 'control': 'ctrl', 'shift': 'shift', 'alt': 'alt', 'meta': 'super', 'super': 'super'}
    modifiers = []
    for token in parts[:-1]:
        modifier = aliases.get(token.lower())
        if modifier is None or modifier in modifiers:
            raise ValueError('Invalid or repeated key modifier')
        modifiers.append(modifier)
    name = parts[-1]
    if not name:
        raise ValueError('Missing key name')
    if _KEYSYM is None:
        library = ctypes.CDLL('libX11.so.6')
        _KEYSYM = library.XStringToKeysym
        _KEYSYM.argtypes = [ctypes.c_char_p]
        _KEYSYM.restype = ctypes.c_ulong
    if not _KEYSYM(name.encode('ascii')):
        raise ValueError('Unknown X11 key ' + name)
    return '+'.join(modifiers + [name])

def validate(actions):
    schema = {'apps': (set(), set()), 'inspect': (set(), {'app', 'query', 'scope', 'limit', 'depth'}), 'read': ({'ref'}, {'offset', 'length'}), 'fill': ({'ref', 'text'}, set()), 'paste': ({'ref', 'text'}, set()), 'invoke': ({'ref', 'action'}, set()), 'focus': ({'ref'}, set()), 'select': ({'ref'}, set()), 'key': ({'ref', 'keys'}, set()), 'wait': ({'app', 'query'}, {'timeout', 'limit'}), 'find': ({'app', 'role', 'as'}, {'name', 'contains', 'scope', 'timeout', 'depth', 'states', 'top'})}
    if not isinstance(actions, list) or len(actions) > 100:
        raise ValueError('Expected an array of at most 100 native operations')
    declared_wait = 0
    bindings = set()
    for a in actions:
        if not isinstance(a, dict) or not isinstance(a.get('op'), str) or a['op'] not in schema:
            raise ValueError('Unknown native operation')
        required, optional = schema[a['op']]
        if required - set(a) or set(a) - required - optional - {'op'}:
            raise ValueError('Invalid fields for ' + a['op'])
        for k in ('app', 'query', 'scope', 'ref', 'text', 'action', 'role', 'name', 'contains', 'as'):
            if k in a and (not isinstance(a[k], str) or '\x00' in a[k] or len(a[k]) > 100000):
                raise ValueError(k + ' must be a string without NUL')
        for k in ('scope', 'ref'):
            if k in a:
                if a[k].startswith('$'):
                    if a[k][1:] not in bindings:
                        raise ValueError('Binding must be defined by an earlier find operation')
                elif not re.fullmatch(REF_PATTERN, a[k]):
                    raise ValueError('Use an observed native reference or earlier $binding')
        for k, lo, hi in [('limit', 1, 500), ('depth', 1, 40), ('offset', 0, 10 ** 7), ('length', 1, 100000)]:
            if k in a and (type(a[k]) is not int or not lo <= a[k] <= hi):
                raise ValueError('Invalid ' + k)
        if 'timeout' in a and (type(a['timeout']) not in (int, float) or not 0 < a['timeout'] <= 60 or (not math.isfinite(a['timeout']))):
            raise ValueError('timeout must be finite, positive and at most 60 seconds')
        if 'top' in a and type(a['top']) is not bool:
            raise ValueError('top must be a boolean')
        if 'states' in a:
            allowed = {'editable', 'focusable', 'focused', 'active', 'modal', 'enabled', 'sensitive', 'selected', 'checked'}
            if not isinstance(a['states'], list) or not a['states'] or any(not isinstance(state, str) or state not in allowed for state in a['states']):
                raise ValueError('states must contain known accessible state names')
        if 'keys' in a:
            if not isinstance(a['keys'], list) or not 1 <= len(a['keys']) <= 100:
                raise ValueError('Expected 1 to 100 key strings')
            for key in a['keys']:
                key_chord(key)
        if a['op'] == 'find':
            if not a['app'].strip() or not a['role'].strip() or not re.fullmatch('[A-Za-z][A-Za-z0-9_]{0,39}', a['as']):
                raise ValueError('find requires app, exact role and a simple binding name')
            if a['as'] in bindings:
                raise ValueError('Binding names must be unique within a batch')
            bindings.add(a['as'])
            declared_wait += a.get('timeout', 3)
        if a['op'] == 'wait':
            if not a['query'].strip():
                raise ValueError('Wait query must not be empty')
            declared_wait += a.get('timeout', 10)
    if declared_wait > 60:
        raise ValueError('Total declared waits must not exceed 60 seconds')
    if len(json.dumps(actions).encode()) > MAX_REQUEST - 1024:
        raise ValueError('Native request too large')
    return actions

class Native:

    def __init__(self):
        import pyatspi
        from gi.repository import Atspi
        Atspi.set_timeout(700, 3000)
        self.api = pyatspi
        self.desktop = pyatspi.Registry.getDesktop(0)
        self.refs = {}
        self.identities = {}
        self.serial = 0
        self.generation = secrets.token_hex(6)
        self.workspace = os.environ['AGENT_DESKTOP_ID']
        from types import SimpleNamespace
        from agent_desktop_fast import Input
        workspace = SimpleNamespace(dir=Path(os.environ['XAUTHORITY']).parent, display=os.environ['DISPLAY'], env=lambda: dict(os.environ))
        self.input = Input(workspace, RuntimeError)

    def owned(self, a):
        # Action resolution always revalidates the process marker freshly.
        return self.pid_owned(a.get_process_id())

    def pid_owned(self, pid):
        try:
            raw = Path('/proc/' + str(pid) + '/environ').read_bytes().split(b'\x00')
            return ('AGENT_DESKTOP_ID=' + self.workspace).encode() in raw
        except OSError:
            return False

    def ref(self, a):
        ident = (a.app.bus_name, a.path)
        if ident not in self.identities:
            self.serial += 1
            self.identities[ident] = 'n' + self.generation + '_' + str(self.serial)
        ref = self.identities[ident]
        self.refs[ref] = (a, a.getRoleName())
        return ref

    def resolve(self, ref):
        if not ref.startswith('n' + self.generation + '_') or ref not in self.refs:
            raise ValueError('Unknown or expired ref; inspect again')
        a, role = self.refs[ref]
        if not self.owned(a) or a.getState().contains(self.api.STATE_DEFUNCT) or a.getRoleName() != role:
            raise ValueError('Stale or foreign target; inspect again')
        return a

    def text(self, a, offset=0, length=12000):
        if a.getRoleName() == 'password text':
            return {'redacted': True}
        try:
            t = a.queryText()
            total = t.characterCount
            return {'text': t.getText(min(offset, total), min(total, offset + length)), 'characters': total, 'truncated': total > offset + length}
        except NotImplementedError:
            return {}

    def observation_identity(self, a, owners):
        bus = a.app.bus_name
        if bus not in owners:
            pid = a.get_process_id()
            owners[bus] = (pid, self.pid_owned(pid))
        return owners[bus]

    def row(self, a, pid=None):
        state = a.getState()
        row = {'ref': self.ref(a), 'role': a.getRoleName(), 'name': a.name[:2000], 'pid': a.get_process_id() if pid is None else pid}
        try:
            actions = a.queryAction()
            row['actions'] = [actions.getName(i) for i in range(actions.nActions)]
        except NotImplementedError:
            pass
        row['state'] = [name for name in ('editable', 'focusable', 'focused', 'active', 'modal', 'enabled', 'sensitive', 'selected', 'checked') if state.contains(getattr(self.api, 'STATE_' + name.upper()))]
        row.update(self.text(a, length=500))
        return row

    def apps(self):
        owners = {}
        rows = []
        for a in self.desktop:
            pid, owned = self.observation_identity(a, owners)
            if owned:
                rows.append(self.row(a, pid))
        return rows

    def inspect(self, app='', query='', scope=None, limit=100, depth=22, selector=None):
        roots = [self.resolve(scope)] if scope else list(self.desktop)
        terms = [s.strip().casefold() for s in query.split('|') if s.strip()]
        out = []
        visited = 0
        truncated = False
        deadline = time.monotonic() + 5
        owners = {}

        def exhausted():
            return visited >= 4000 or len(out) >= limit or time.monotonic() >= deadline

        def visit(a, level):
            nonlocal visited, truncated
            if exhausted():
                truncated = True
                return
            visited += 1
            try:
                pid, owned = self.observation_identity(a, owners)
                if not owned:
                    return
                if level == 0 and (not scope) and app and (a.name != app) and (str(pid) != app):
                    return
                state = a.getState()
                role = a.getRoleName()
                if (level or (scope and selector is not None)) and role != 'application' and (not state.contains(self.api.STATE_SHOWING)):
                    return
                name = a.name
                useful = selector is not None or bool(name) or role in ('text', 'entry', 'password text', 'combo box', 'dialog', 'frame', 'application')
                if useful:
                    # For precise local lookup, avoid fetching text/actions on every unrelated node.
                    if selector and (role != selector['role'] or ('name' in selector and name != selector['name']) or (selector.get('top') and (a.parent is None or a.parent.getRoleName() != 'application'))):
                        row = None
                    else:
                        row = self.row(a, pid)
                    if row is not None and (not selector or all(state in row['state'] for state in selector.get('states', []))) and (not selector or 'contains' not in selector or selector['contains'] in (name + '\n' + row.get('text', ''))) and (not terms or any((t in (role + ' ' + name + ' ' + row.get('text', '')).casefold() for t in terms))):
                        out.append(row)
                if selector and selector.get('top') and role != 'application':
                    return
                if level >= depth:
                    if a.childCount:
                        truncated = True
                    return
                count = a.childCount
                for i in range(count):
                    if exhausted():
                        truncated = True
                        break
                    child = a.getChildAtIndex(i)
                    if child is not None:
                        visit(child, level + 1)
                    else:
                        truncated = True
            except Exception:
                if level == 0:
                    raise
                truncated = True
        for a in roots:
            if exhausted():
                truncated = True
                break
            visit(a, 0)
        return {'nodes': out, 'visited': visited, 'truncated': truncated}

    def activate(self, a):
        pid = str(a.get_process_id())
        top = a
        # Toolkit containers may also use frame/dialog roles inside a real window.
        # The application child identifies the actual top-level window, including choosers.
        for _ in range(60):
            parent = top.parent
            if parent is None or parent.getRoleName() == 'application':
                break
            top = parent
        lines = subprocess.check_output(['wmctrl', '-lp'], text=True).splitlines()
        candidates = [(x.split(None, 4) + [''])[:5] for x in lines if len(x.split(None, 4)) >= 4 and x.split(None, 4)[2] == pid]
        exact = [x for x in candidates if x[4] == top.name]
        candidates = exact or candidates
        if len(candidates) != 1:
            # Some modal alerts have an accessible name but an empty WM title.
            # Match observed geometry only, and reject non-unique candidates.
            extent = top.queryComponent().getExtents(self.api.DESKTOP_COORDS)
            expected = tuple(extent) if not hasattr(extent, 'x') else (extent.x, extent.y, extent.width, extent.height)
            window_ids = {candidate[0] for candidate in candidates}
            geometry_lines = subprocess.check_output(['wmctrl', '-lpG'], text=True).splitlines()
            matched = []
            for line in geometry_lines:
                fields = line.split(None, 8)
                if len(fields) < 8 or fields[0] not in window_ids or fields[2] != pid:
                    continue
                geometry = tuple(int(value) for value in fields[3:7])
                if len(expected) == 4 and all(abs(actual - observed) <= 2 for actual, observed in zip(geometry, expected)):
                    matched.append(fields[0])
            if len(matched) != 1:
                raise ValueError('Ambiguous app window; title and observed geometry did not identify a unique window')
            candidates = [[matched[0]]]

        xid = candidates[0][0]
        subprocess.run(['wmctrl', '-ia', xid], check=True)
        end = time.monotonic() + 2
        while time.monotonic() < end:
            active = subprocess.run(['xdotool', 'getactivewindow'], capture_output=True, text=True)
            if active.returncode == 0 and int(active.stdout.strip()) == int(xid, 16):
                return
            time.sleep(0.02)
        raise TimeoutError('Window did not become active')

    def run(self, a):
        op = a['op']
        if op == 'apps':
            return self.apps()
        if op == 'inspect':
            return self.inspect(**{k: v for k, v in a.items() if k != 'op'})
        if op == 'find':
            if a.get('scope'):
                application = self.resolve(a['scope'])
                for _ in range(60):
                    if application.getRoleName() == 'application':
                        break
                    application = application.parent
                    if application is None:
                        raise ValueError('Scope has no application identity')
                if application.getRoleName() != 'application' or a['app'] not in (application.name, str(application.get_process_id())):
                    raise ValueError('Scope does not belong to the requested app')
            end = time.monotonic() + a.get('timeout', 3)
            while True:
                result = self.inspect(app=a['app'], scope=a.get('scope'), limit=3, depth=a.get('depth', 40), selector=a)
                if len(result['nodes']) > 1:
                    raise ValueError('Ambiguous native selector; use a scope or exact name')
                if len(result['nodes']) == 1 and not result['truncated']:
                    return result['nodes'][0]
                if result['truncated']:
                    raise ValueError('Incomplete native lookup; narrow app/scope or increase depth')
                if time.monotonic() >= end:
                    raise TimeoutError('Native selector did not match ' + str({k: a[k] for k in ('app', 'role', 'name', 'contains') if k in a}))
                time.sleep(0.025)
        if op == 'wait':
            end = time.monotonic() + a.get('timeout', 10)
            while time.monotonic() < end:
                result = self.inspect(app=a['app'], query=a['query'], limit=a.get('limit', 60))
                if result['nodes']:
                    return result
                time.sleep(0.08)
            raise TimeoutError('No matching native control')
        target = self.resolve(a['ref'])
        if op == 'read':
            return self.row(target) | self.text(target, a.get('offset', 0), a.get('length', 12000))
        if op == 'select':
            # GTK file lists expose filename cells nested inside selectable row cells.
            # Select through the nearest ancestor's Selection interface, not focus alone.
            self.activate(target)
            child = target
            for _ in range(12):
                parent = child.parent
                if parent is None or not self.owned(parent):
                    break
                try:
                    selection = parent.querySelection()
                except NotImplementedError:
                    child = parent
                    continue
                index = child.getIndexInParent()
                if index < 0 or not selection.clearSelection():
                    raise RuntimeError('Application refused exclusive selection; inspect before continuing')
                if not selection.selectChild(index) or not selection.isChildSelected(index):
                    # GtkTreeView can expose Selection but refuse selectChild for rows.
                    # Its Table interface maps the observed child index to a native row.
                    try:
                        table = parent.queryTable()
                        row = table.getRowAtIndex(index)
                        if not 0 <= row < table.nRows or not table.addRowSelection(row) or list(table.getSelectedRows()) != [row]:
                            raise RuntimeError('Table row selection was not verified')
                    except NotImplementedError:
                        raise RuntimeError('Application refused exclusive selection; inspect before continuing') from None
                return {'selected': a['ref'], 'selection_container': self.ref(parent)}
            raise ValueError('No supported selection container for target')
        if op == 'fill':
            if target.getRoleName() == 'password text':
                raise ValueError('Password entry requires a separately authorized input route')
            state = target.getState()
            if not all((state.contains(x) for x in (self.api.STATE_EDITABLE, self.api.STATE_ENABLED, self.api.STATE_SENSITIVE))):
                raise ValueError('Target is not editable and enabled')
            if not target.queryEditableText().setTextContents(a['text']):
                raise RuntimeError('Application refused text update')
            return {'filled': a['ref'], 'characters': len(a['text'])}
        if op == 'invoke':
            if not target.getState().contains(self.api.STATE_SENSITIVE):
                raise ValueError('Target is disabled')
            actions = target.queryAction()
            names = [actions.getName(i) for i in range(actions.nActions)]
            if a['action'] not in names:
                raise ValueError('Action unavailable; observed actions: ' + str(names))
            self.activate(target)
            if not actions.doAction(names.index(a['action'])):
                raise RuntimeError('Application refused action')
            return {'invoked': a['ref'], 'action': a['action']}
        if op in ('focus', 'key', 'paste'):
            self.activate(target)
            if target.getRoleName() not in ('frame', 'dialog', 'application', 'window', 'file chooser', 'alert'):
                if not target.queryComponent().grabFocus():
                    raise RuntimeError('Application refused focus')
            if op == 'key':
                self.input.key([key_chord(key) for key in a['keys']])
            if op == 'paste':
                self.input.paste(a['text'])
            return {'focused': a['ref'], **({'keys': a['keys']} if op == 'key' else {})}
        raise ValueError('Unknown operation')

def read_frame(conn, limit):
    data = bytearray()
    while True:
        chunk = conn.recv(min(65536, limit + 1 - len(data)))
        if not chunk:
            raise ConnectionError('Native connection closed before a complete frame')
        data.extend(chunk)
        if len(data) > limit:
            raise ValueError('Native frame exceeds size limit')
        if b'\n' in chunk:
            line, extra = bytes(data).split(b'\n', 1)
            if extra:
                raise ValueError('Only one native frame is allowed')
            return json.loads(line)

def send_frame(conn, value, limit):
    data = (json.dumps(value, ensure_ascii=False) + '\n').encode()
    if len(data) > limit:
        raise ValueError('Native frame exceeds size limit')
    conn.sendall(data)

def process_environment(pid):
    try:
        return Path('/proc/' + str(pid) + '/environ').read_bytes().split(b'\x00')
    except OSError:
        return []

def connect_backend(path, workspace, bus):
    conn = socket.socket(socket.AF_UNIX)
    try:
        conn.settimeout(8)
        conn.connect(str(path))
        pid, uid, _ = struct.unpack('3i', conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize('3i')))
        env = process_environment(pid)
        if uid != os.getuid() or ('AGENT_DESKTOP_ID=' + workspace).encode() not in env or ('DBUS_SESSION_BUS_ADDRESS=' + bus).encode() not in env:
            raise ValueError('Native backend process belongs to a different workspace')
        hello = read_frame(conn, 4096)
        if not isinstance(hello, dict) or hello.get('workspace') != workspace or hello.get('bus') != bus or (hello.get('pid') != pid) or (not re.fullmatch('[0-9a-f]{12}', str(hello.get('generation', '')))):
            raise ValueError('Native backend handshake identity mismatch')
        conn.settimeout(90)
        return (conn, hello)
    except BaseException:
        conn.close()
        raise

def execute(actions, native, completed):
    bindings = {}
    for action in actions:
        item = dict(action)
        for field in ('ref', 'scope'):
            if field in item and item[field].startswith('$'):
                item[field] = bindings[item[field][1:]]
        result = native.run(item)
        completed.append(result)
        if item['op'] == 'find':
            bindings[item['as']] = result['ref']

def handle_connection(conn, native):
    completed = []
    start = time.perf_counter()
    try:
        conn.settimeout(5)
        _, uid, _ = struct.unpack('3i', conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize('3i')))
        if uid != os.getuid():
            raise ValueError('Native client uid mismatch')
        send_frame(conn, {'workspace': native.workspace, 'generation': native.generation, 'bus': os.environ['DBUS_SESSION_BUS_ADDRESS'], 'pid': os.getpid()}, 4096)
        request = read_frame(conn, MAX_REQUEST)
        if not isinstance(request, dict) or set(request) != {'workspace', 'generation', 'actions'} or request['workspace'] != native.workspace or (request['generation'] != native.generation):
            raise ValueError('Native request workspace or generation mismatch')
        actions = validate(request['actions'])
        for item in actions:
            for field in ('ref', 'scope'):
                if field in item and not item[field].startswith('$') and (not item[field].startswith('n' + native.generation + '_')):
                    raise ValueError('Expired native reference; inspect again')
        conn.settimeout(5)
        execute(actions, native, completed)
        result = {'completed': len(completed), 'results': completed}
    except Exception as error:
        result = {'completed': len(completed), 'results': completed, 'error': str(error)}
    result['elapsed_ms'] = (time.perf_counter() - start) * 1000
    try:
        send_frame(conn, result, MAX_RESPONSE)
    except (OSError, ValueError):
        pass

def serve(path):
    native = Native()
    server = socket.socket(socket.AF_UNIX)
    server.bind(path)
    os.chmod(path, 384)
    server.listen(4)
    try:
        while True:
            conn, _ = server.accept()
            with conn:
                handle_connection(conn, native)
    finally:
        server.close()

def run_cli(a, w, Fail):
    if not w.state.get('a11y') or w.state.get('share_home'):
        raise Fail('Native controls require a private workspace created with new --a11y')
    actions = validate(json.loads(a.actions if a.actions is not None else sys.stdin.read()))
    path = w.dir / 'native.sock'
    bus = w.env()['DBUS_SESSION_BUS_ADDRESS']
    workspace = w.state['id']
    import fcntl
    with (w.dir / 'native.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        connection = None
        if path.exists():
            try:
                connection = connect_backend(path, workspace, bus)
            except OSError as error:
                # Recover only refused/dead sockets before submitting any request.
                if error.errno not in (errno.ECONNREFUSED, errno.ENOENT):
                    raise
                if path.exists():
                    if not stat.S_ISSOCK(path.lstat().st_mode):
                        raise Fail('Native socket path is not a socket')
                    path.unlink()
        if connection is None:
            log = w.files / 'logs' / 'native.log'
            with log.open('a') as output:
                process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--serve', str(path)], env=w.env(), stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
            end = time.monotonic() + 8
            while not path.exists():
                if process.poll() is not None or time.monotonic() > end:
                    raise Fail('Native backend did not start; inspect ' + str(log))
                time.sleep(0.02)
            connection = connect_backend(path, workspace, bus)
    w.touch()
    conn, hello = connection
    with conn:
        for item in actions:
            for field in ('ref', 'scope'):
                if field in item and not item[field].startswith('$') and (not item[field].startswith('n' + hello['generation'] + '_')):
                    raise Fail('Expired native reference; inspect again')
        # Never retry after submission: input may already have been applied.
        send_frame(conn, {'workspace': workspace, 'generation': hello['generation'], 'actions': actions}, MAX_REQUEST)
        result = read_frame(conn, MAX_RESPONSE)
    print(json.dumps(result, ensure_ascii=False))
    if 'error' in result:
        raise Fail('Native batch failed; inspect completed results before retrying')
if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--serve', required=True)
    serve(parser.parse_args().serve)

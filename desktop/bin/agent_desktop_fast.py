"""Native batched X11 input for agent-desktop; only allocated workspaces are accepted.

libxdo is already installed with xdotool. No new service, packages, network listener,
or owner's desktop connection is needed. session keeps this runtime alive over JSONL.
"""
import contextlib
import ctypes as C
import json
import math
import os
import subprocess
import sys
import time
from types import SimpleNamespace


class Input:
    def __init__(self, workspace, fail):
        self.w, self.fail = workspace, fail
        self.ctx = self.display = None
        try:
            self.x = C.CDLL('libX11.so.6')
            self.lib = C.CDLL('libxdo.so.3')
        except OSError as e:
            raise fail(f'native input unavailable: {e}; use legacy commands')
        ptr, integer, window = C.c_void_p, C.c_int, C.c_ulong
        def bind(lib, name, result, *args):
            f = getattr(lib, name); f.restype = result; f.argtypes = list(args)
        bind(self.x, 'XOpenDisplay', ptr, C.c_char_p)
        bind(self.x, 'XCloseDisplay', integer, ptr)
        bind(self.x, 'XSync', integer, ptr, integer)
        bind(self.lib, 'xdo_new_with_opened_display', ptr, ptr, C.c_char_p, integer)
        bind(self.lib, 'xdo_free', None, ptr)
        bind(self.lib, 'xdo_move_mouse', integer, ptr, integer, integer, integer)
        for n in ('xdo_mouse_down', 'xdo_mouse_up'):
            bind(self.lib, n, integer, ptr, window, integer)
        for n in ('xdo_send_keysequence_window', 'xdo_enter_text_window'):
            bind(self.lib, n, integer, ptr, window, C.c_char_p, C.c_uint)
        bind(self.lib, 'xdo_get_active_modifiers', integer, ptr, C.POINTER(ptr), C.POINTER(integer))
        for n in ('xdo_clear_active_modifiers', 'xdo_set_active_modifiers'):
            bind(self.lib, n, integer, ptr, window, ptr, integer)
        self.libc = C.CDLL(None)
        bind(self.libc, 'free', None, ptr)
        old_auth = os.environ.get('XAUTHORITY')
        os.environ['XAUTHORITY'] = str(workspace.dir / 'Xauthority')
        try:
            self.display = self.x.XOpenDisplay(workspace.display.encode())
            if not self.display:
                raise fail('cannot open workspace X display')
            self.ctx = self.lib.xdo_new_with_opened_display(self.display, workspace.display.encode(), 0)
            if not self.ctx:
                self.x.XCloseDisplay(self.display); self.display = None
                raise fail('cannot initialize native workspace input')
        finally:
            if old_auth is None: os.environ.pop('XAUTHORITY', None)
            else: os.environ['XAUTHORITY'] = old_auth

    def close(self):
        if self.ctx:
            self.lib.xdo_free(self.ctx); self.ctx = None
        if self.display:
            self.x.XCloseDisplay(self.display); self.display = None

    def call(self, name, *args):
        if getattr(self.lib, name)(self.ctx, *args) != 0:
            raise self.fail(f'{name} failed')

    def sync(self):
        self.x.XSync(self.display, 0)

    @contextlib.contextmanager
    def modifiers(self):
        keys, count = C.c_void_p(), C.c_int()
        self.call('xdo_get_active_modifiers', C.byref(keys), C.byref(count))
        try:
            self.call('xdo_clear_active_modifiers', 0, keys, count.value)
            yield
        finally:
            self.lib.xdo_set_active_modifiers(self.ctx, 0, keys, count.value)
            self.libc.free(keys)

    def key(self, keys, delay=0):
        with self.modifiers():
            for key in keys:
                self.call('xdo_send_keysequence_window', 0, key.encode(), int(delay * 1000))
        self.sync()

    def move(self, x, y):
        # XSync acknowledges the warp without waiting for a movement that may not occur.
        self.call('xdo_move_mouse', x, y, 0)
        self.sync()

    def click(self, button=1, count=1, interval=40):
        # Keep a brief physical press; application throughput still governs intervals.
        for i in range(count):
            self.call('xdo_mouse_down', 0, button)
            self.sync()
            time.sleep(0.001)
            self.call('xdo_mouse_up', 0, button)
            self.sync()
            if i + 1 < count: time.sleep(interval / 1000)

    def type(self, text, delay=0):
        # Keep the established Unicode keymap safety delay; paste avoids remapping.
        import re
        with self.modifiers():
            for seg in re.findall(r'[\x20-\x7e\t\n]+|[^\x20-\x7e\t\n]+', text):
                ms = delay if seg.isascii() else max(delay, 80)
                self.call('xdo_enter_text_window', 0, seg.encode(), int(ms * 1000))
        self.sync()

    def paste(self, text, key='ctrl+v'):
        try:
            # xclip owns and services the selection after this process returns.
            # Browsers may make several asynchronous selection requests per paste.
            subprocess.run(['xclip', '-selection', 'clipboard', '-in'], input=text.encode(),
                           env=self.w.env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           check=True, timeout=5)
        except (OSError, subprocess.CalledProcessError) as e:
            raise self.fail(f'clipboard paste unavailable: {e}; use type')
        self.key([key])
        # Permit asynchronous client insertion before another paste replaces the data.
        time.sleep(0.05)


# Validate the entire batch before sending any input. JSON is data, never shell code.
FIELDS = {
    'click': {'x','y','button','count','interval'}, 'move': {'x','y'},
    'key': {'keys','delay'}, 'type': {'text','delay'}, 'paste': {'text','key'},
    'scroll': {'direction','amount','x','y','interval'},
    'drag': {'x1','y1','x2','y2','button','duration','steps'},
    'wait': {'seconds'}, 'wait_window': {'name','class','pid','timeout'},
    'focus': {'window','name','class','pid'},
    'screenshot': {'output','region','scale','window'},
    'wait_stable': {'region','timeout','interval','samples'},
}
REQUIRED = {'click':('x','y'), 'move':('x','y'), 'key':('keys',),
            'type':('text',), 'paste':('text',), 'scroll':('direction',),
            'drag':('x1','y1','x2','y2'), 'wait':('seconds',)}


def validate(actions, fail):
    def bad(message): raise fail(message, 2)
    if not isinstance(actions, list) or len(actions) > 10000:
        bad('actions must be a JSON array with at most 10000 items')
    for i, a in enumerate(actions):
        if not isinstance(a, dict) or not isinstance(a.get('action'), str) or a['action'] not in FIELDS:
            bad(f'action {i}: unknown or missing action')
        kind = a['action']
        if set(a) - FIELDS[kind] - {'action'}: bad(f'action {i}: unknown fields')
        if any(k not in a for k in REQUIRED.get(kind, ())): bad(f'action {i}: missing required fields')
        for k,v in a.items():
            if k in ('x','y','x1','y1','x2','y2','button','count','amount','steps','pid','samples'):
                if type(v) is not int or abs(v) > (2**31-1 if k == 'pid' else 100000): bad(f'action {i}: invalid {k}')
                if k in ('button','count','amount','steps','pid','samples') and v < 1: bad(f'action {i}: {k} must be positive')
            if k in ('delay','interval','duration','seconds','timeout','scale'):
                if type(v) not in (int,float) or v < 0 or v > 60000 or not math.isfinite(v):
                    bad(f'action {i}: invalid {k}')
                if (k in ('scale','timeout') or (k == 'interval' and kind == 'wait_stable')) and v == 0: bad(f'action {i}: {k} must be positive')
            if k in ('text','key','direction','window','name','class','output','region'):
                if not isinstance(v,str) or '\0' in v: bad(f'action {i}: {k} must be a string without NUL')
        if kind == 'key' and (not isinstance(a['keys'],list) or not a['keys'] or
                any(not isinstance(k,str) or not k or '\0' in k for k in a['keys'])):
            bad(f'action {i}: keys must be a nonempty array of strings')
        if kind == 'scroll':
            if a['direction'] not in ('up','down','left','right'): bad(f'action {i}: invalid direction')
            if ('x' in a) != ('y' in a): bad(f'action {i}: scroll needs both x and y')
        if kind in ('wait_window','focus'):
            criteria = sum(k in a for k in ('name','class','pid')) + (kind == 'focus' and 'window' in a)
            if criteria != 1: bad(f'action {i}: give exactly one window selector')
        if 'region' in a:
            try:
                x,y,w,h = map(int,a['region'].split(','))
                if min(x,y) < 0 or min(w,h) <= 0: raise ValueError()
            except ValueError: bad(f'action {i}: region must be X,Y,W,H with positive size')
    return actions


class Runner:
    def __init__(self, w, fail, screenshot, wait, focus):
        self.w, self.fail = w, fail
        self.screenshot, self.wait, self.focus = screenshot, wait, focus
        self.native = None

    def close(self):
        if self.native: self.native.close()

    def grab(self, action):
        # Reuse PIL imports in sessions. Native input and Pillow use the same private Xauth.
        import io
        from contextlib import redirect_stdout
        opts = dict(ws=self.w.name, output=None, window=None, region=None, scale=None)
        opts.update({k:v for k,v in action.items() if k != 'action'})
        with redirect_stdout(io.StringIO()) as out:
            self.screenshot(SimpleNamespace(**opts))
        return out.getvalue().strip()

    def stable(self, a):
        from PIL import ImageGrab
        os.environ['XAUTHORITY'] = str(self.w.dir / 'Xauthority')
        box = None
        if a.get('region'):
            x,y,w,h = map(int,a['region'].split(',')); box=(x,y,x+w,y+h)
        deadline = time.monotonic() + a.get('timeout',5)
        previous, equal = None, 0
        while time.monotonic() < deadline:
            self.w.touch()
            time.sleep(a.get('interval',0.05))
            img = ImageGrab.grab(bbox=box, xdisplay=self.w.display)
            pixels = img.tobytes()
            equal = equal+1 if pixels == previous else 0
            if equal >= a.get('samples',2): return
            previous = pixels
        raise self.fail('screen did not stabilize before timeout',4)

    def run(self, actions, screenshot=True, shot_options=None):
        validate(actions, self.fail)
        if shot_options: validate([dict(action='screenshot', **shot_options)], self.fail)
        if not self.w.server_alive(): raise self.fail('workspace X server has exited')
        start = time.perf_counter(); completed=0; shots=[]; results=[]
        try:
            for a in actions:
                self.w.touch(); k=a['action']
                if k in ('click','move','key','type','paste','scroll','drag'):
                    if self.native is None: self.native = Input(self.w,self.fail)
                    n=self.native
                if k in ('click','move'):
                    n.move(a['x'],a['y'])
                    if k == 'click': n.click(a.get('button',1),a.get('count',1),a.get('interval',40))
                elif k == 'key': n.key(a['keys'],a.get('delay',0))
                elif k == 'type': n.type(a['text'],a.get('delay',0))
                elif k == 'paste': n.paste(a['text'],a.get('key','ctrl+v'))
                elif k == 'scroll':
                    if 'x' in a: n.move(a['x'],a['y'])
                    n.click({'up':4,'down':5,'left':6,'right':7}[a['direction']],a.get('amount',3),a.get('interval',0))
                elif k == 'drag':
                    n.move(a['x1'],a['y1']); button=a.get('button',1)
                    n.call('xdo_mouse_down',0,button)
                    try:
                        steps=a.get('steps',8); duration=a.get('duration',0.12)
                        for i in range(1,steps+1):
                            time.sleep(duration/steps)
                            n.move(round(a['x1']+(a['x2']-a['x1'])*i/steps),round(a['y1']+(a['y2']-a['y1'])*i/steps))
                    finally: n.call('xdo_mouse_up',0,button); n.sync()
                elif k == 'wait': time.sleep(a['seconds'])
                elif k == 'wait_stable': self.stable(a)
                elif k == 'screenshot': shots.append(self.grab(a))
                elif k in ('wait_window','focus'):
                    import io
                    from contextlib import redirect_stdout
                    opts=dict(ws=self.w.name,window=None,name=None,cls=None,pid=None,timeout=30)
                    opts.update({('cls' if key=='class' else key):v for key,v in a.items() if key!='action'})
                    with redirect_stdout(io.StringIO()) as out:
                        (self.wait if k=='wait_window' else self.focus)(SimpleNamespace(**opts))
                    results.append({'index':completed,'result':out.getvalue().strip()})
                completed+=1
            if screenshot and (not actions or actions[-1]['action'] != 'screenshot'):
                # Server acknowledgment does not imply the client has painted yet.
                time.sleep(0.03)
                shots.append(self.grab(shot_options or {}))
        except Exception as e:
            # Do not replay a partly completed batch automatically.
            raise self.fail(f'batch stopped after {completed}/{len(actions)} actions (failing action may have side effects): {e}',getattr(e,'code',1)) from e
        return dict(completed=completed,elapsed_ms=round((time.perf_counter()-start)*1000,3),
                    screenshots=shots,results=results)


def run_cli(a,w,fail,screenshot,wait,focus):
    runner=Runner(w,fail,screenshot,wait,focus)
    def decode(raw):
        try: return json.loads(raw)
        except (ValueError,TypeError) as e: raise fail(f'invalid action JSON: {e}',2)
    try:
        if a.session:
            print(json.dumps({'ready':True,'workspace':w.name}),flush=True)
            for line in sys.stdin:
                if not line.strip(): continue
                try:
                    request=decode(line)
                    if isinstance(request,list): request={'actions':request}
                    if not isinstance(request,dict) or set(request)-{'actions','screenshot'}:
                        raise fail('session expects an action array or {actions, screenshot}',2)
                    if 'screenshot' in request and type(request['screenshot']) is not bool:
                        raise fail('screenshot must be a boolean',2)
                    result=runner.run(request.get('actions'),request.get('screenshot',not a.no_screenshot),
                                      {k:getattr(a,k) for k in ('output','region','scale') if getattr(a,k,None) is not None})
                except fail as e:
                    result={'error':str(e),'code':e.code}
                print(json.dumps(result),flush=True)
        else:
            if getattr(a,'paste',False):
                text=sys.stdin.read() if a.stdin else a.text
                if text is None: raise fail('give TEXT or --stdin',2)
                actions=[{'action':'paste','text':text,'key':a.key}]
            else: actions=decode(a.actions if a.actions is not None else sys.stdin.read())
            result=runner.run(actions,not a.no_screenshot,
                              {k:getattr(a,k) for k in ('output','region','scale') if getattr(a,k,None) is not None})
            print(json.dumps(result))
    finally: runner.close()

"""Verified NDJSON engine benchmark; no model time or Codex baseline invented."""
import argparse
import base64
import json
import os
import platform
from pathlib import Path
import re
import selectors
import shlex
import statistics
import subprocess
import time

parser = argparse.ArgumentParser()
parser.add_argument('--command', default=os.environ.get('CUA_ENGINE', 'target/debug/linux-cua'))
parser.add_argument('--output', required=True)
parser.add_argument('--iterations', type=int, default=20)
parser.add_argument('--label', default='linux-cua')
parser.add_argument('--screenshot')
args = parser.parse_args()
if args.iterations < 1:
    parser.error('iterations must be positive')
if not os.environ.get('CUA_DISPLAY') or os.environ['CUA_DISPLAY'].split('.')[0] == ':0':
    parser.error('CUA_DISPLAY must select an owned private display')

samples = []
proc = subprocess.Popen(shlex.split(args.command), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
selector = selectors.DefaultSelector()
selector.register(proc.stdout, selectors.EVENT_READ)

def rpc(method, params=None, expected_error=None, record=True):
    start = time.perf_counter()
    proc.stdin.write(json.dumps({'id': len(samples), 'method': method, 'params': params or {}}) + '\n')
    proc.stdin.flush()
    if not selector.select(25):
        raise TimeoutError(method)
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError(f'Engine closed during {method}')
    reply = json.loads(line)
    elapsed = (time.perf_counter() - start) * 1000
    if expected_error:
        assert reply.get('error', {}).get('code') == expected_error, reply
        return reply
    if 'error' in reply:
        raise RuntimeError(reply['error'])
    result = reply['result']
    if record:
        samples.append({'method': method, 'tool_ms': elapsed, 'bytes': len(line.encode()),
                        'timing': result.get('timing') if isinstance(result, dict) else None})
    return result

def element(state, role, name):
    for line in state['accessibility'].splitlines():
        if role in line and name in line:
            found = re.match(r'\s*\[?(\d+)', line)
            if found:
                return int(found[1])
    raise AssertionError(f'Cannot find {role} {name}: {state}')

try:
    windows = rpc('list_windows', record=False)
    fixture = next(w for w in windows if w['title'] == 'Linux CUA Benchmark')
    target = {'window_id': fixture['window_id']}
    rpc('get_app_state', target, expected_error='approval_required', record=False)
    rpc('approve_app', {**target, 'app_id': fixture['app_id']}, record=False)
    initial = rpc('get_app_state', target)
    assert initial['accessibility'], initial
    field = element(initial, 'text', 'Message')
    button = element(initial, 'button', 'Apply')
    tasks = []
    for i in range(args.iterations):
        start = time.perf_counter()
        message = f'Iteration {i}: Grüße λ {i * 7 + 3}'
        state = rpc('set_value', {**target, 'element_index': field, 'value': message})
        assert message in state['accessibility'], state
        state = rpc('click', {**target, 'element_index': button})
        assert 'Applied: ' + message in state['accessibility'], state
        tasks.append({'task': 'edit-and-apply', 'success': True,
                      'wall_ms': (time.perf_counter() - start) * 1000})
        screenshot = rpc('get_screenshot', target)
    image = base64.b64decode(screenshot['data'])
    assert image.startswith(b'\x89PNG\r\n\x1a\n')
    assert screenshot['width'] == fixture['width']
    if args.screenshot:
        Path(args.screenshot).write_bytes(image)
    rpc('turn_ended', record=False)
    rpc('get_app_state', target, expected_error='approval_required', record=False)
    summary = {}
    for method in sorted({s['method'] for s in samples}):
        values = sorted(s['tool_ms'] for s in samples if s['method'] == method)
        summary[method] = {'n': len(values), 'median_ms': statistics.median(values),
                           'p95_ms': values[min(len(values)-1, int(len(values)*.95))]}
    report = {'schema': 1, 'label': args.label, 'suite': 'gtk-edit-apply-v1',
              'scope': 'local engine, no model inference', 'model': None,
              'platform': platform.platform(), 'window_size': [fixture['width'], fixture['height']],
              'command': args.command,
              'codex_comparison': 'not measured', 'samples': samples, 'tasks': tasks,
              'summary': summary, 'success': True}
    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(summary, indent=2))
finally:
    proc.stdin.close()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.terminate()
        proc.wait(timeout=5)

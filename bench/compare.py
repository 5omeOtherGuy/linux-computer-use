"""Compare compatible measured reports, never a configured settle constant."""
import argparse
import json
import math
import statistics
from collections import Counter
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('candidate')
p.add_argument('reference')
a = p.parse_args()
c, r = [json.loads(Path(path).read_text()) for path in [a.candidate, a.reference]]
for key in ['schema', 'suite', 'scope', 'model', 'window_size']:
    if key not in c or key not in r or c[key] != r[key]:
        p.error(f'{key} differs or is absent: reports are not comparable')
for report in [c, r]:
    if report.get('success') is not True or not report.get('tasks'):
        p.error('Both reports must contain successful, independently verified task runs')
    for task in report['tasks']:
        if task.get('success') is not True or not isinstance(task.get('wall_ms'), (int, float)) or not math.isfinite(task['wall_ms']) or task['wall_ms'] <= 0:
            p.error('Invalid task timing or failed task')
if Counter(t['task'] for t in c['tasks']) != Counter(t['task'] for t in r['tasks']):
    p.error('Task sets or repetition counts differ')
cm = statistics.median(t['wall_ms'] for t in c['tasks'])
rm = statistics.median(t['wall_ms'] for t in r['tasks'])
print(json.dumps({'scope': c['scope'], 'candidate': c['label'], 'reference': r['label'],
                  'candidate_median_ms': cm, 'reference_median_ms': rm,
                  'candidate_over_reference': cm/rm,
                  'interpretation': 'Ratio below 1 is faster on this measured task set only; not a general speed claim.'}, indent=2))

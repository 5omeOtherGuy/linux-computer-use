"""Compare an archived baseline CLI with native batches and a warm JSONL session.

Requires the synthetic fast_fixture.py already open/maximized on --workspace.
No real applications, credentials, owner display, or external services are used.
"""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import time

CLI=str(Path(__file__).resolve().parents[1]/'bin/agent-desktop')

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--workspace',required=True)
    ap.add_argument('--state',required=True,type=Path)
    ap.add_argument('--baseline',required=True)
    ap.add_argument('--output',required=True,type=Path)
    ap.add_argument('--samples',type=int,default=7)
    a=ap.parse_args()
    def command(cli,*args):
        r=subprocess.run([cli,*args],capture_output=True,text=True,timeout=35)
        if r.returncode: raise RuntimeError(r.stderr)
        return r.stdout.strip()
    session=subprocess.Popen([CLI,'session',a.workspace],stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
    def persistent(actions,screenshot=True):
        session.stdin.write(json.dumps(dict(actions=actions,screenshot=screenshot))+'\n')
        session.stdin.flush()
        result=json.loads(session.stdout.readline())
        if 'error' in result: raise RuntimeError(result['error'])
        return result
    rows=[]
    try:
        assert json.loads(session.stdout.readline())['ready']
        persistent([dict(action='move',x=50,y=50)])  # explicit warm-up excluded from timing
        for name,text in [('ascii_120','fast reliable input '*6),('unicode_55','Grüße 世界 🚀 '*5)]:
            timings={m:[] for m in ('legacy','batch','session')}
            passes={m:0 for m in timings}
            actions=[dict(action='move',x=210,y=20),dict(action='click',x=200,y=15),
                     dict(action='key',keys=['ctrl+a']),dict(action='paste',text=text),
                     dict(action='key',keys=['Return'])]
            for i in range(a.samples):
                # Rotate ordering to reduce warm-cache and host-load bias.
                modes=list(timings); modes=modes[i%3:]+modes[:i%3]
                for mode in modes:
                    t=time.perf_counter()
                    if mode=='legacy':
                        c=a.baseline; w=a.workspace
                        command(c,'move',w,'210','20')  # avoids old same-position sync hang
                        command(c,'click',w,'200','15');command(c,'key',w,'ctrl+a')
                        command(c,'type',w,text);command(c,'key',w,'Return');command(c,'screenshot',w)
                    elif mode=='batch': command(CLI,'batch',a.workspace,json.dumps(actions))
                    else: persistent(actions)
                    timings[mode].append((time.perf_counter()-t)*1000)
                    passes[mode]+=json.loads(a.state.read_text())['submitted_entry']==text
            for mode,values in timings.items():
                row=dict(scenario=name,mode=mode,n=a.samples,correct=passes[mode],
                         median_ms=statistics.median(values),min_ms=min(values),max_ms=max(values),samples_ms=values)
                rows.append(row)
                print(name,mode,round(row['median_ms'],2),'ms',f'{passes[mode]}/{a.samples}',flush=True)
        # Isolate screenshot transport-ready files, without model/image-upload time.
        for mode,cli in [('legacy',a.baseline),('current',CLI)]:
            values=[];sizes=[]
            for i in range(a.samples):
                t=time.perf_counter();path=command(cli,'screenshot',a.workspace)
                values.append((time.perf_counter()-t)*1000);sizes.append(Path(path).stat().st_size)
            rows.append(dict(scenario='screenshot',mode=mode,n=a.samples,median_ms=statistics.median(values),
                             median_bytes=statistics.median(sizes),samples_ms=values))
        # Motor speed without screenshots; the UI counter proves all clicks arrived.
        before=json.loads(a.state.read_text())['clicks']
        t=time.perf_counter()
        r=persistent([dict(action='click',x=200,y=780) for _ in range(100)],False)
        elapsed=(time.perf_counter()-t)*1000
        deadline=time.monotonic()+3
        while time.monotonic()<deadline:
            count=json.loads(a.state.read_text())['clicks']-before
            if count==100: break
            time.sleep(.005)
        rows.append(dict(scenario='100_clicks',mode='session',n=1,dispatch_ms=elapsed,delivered_ms=(time.perf_counter()-t)*1000,correct=count))
        a.output.write_text(json.dumps(dict(results=rows,notes=[
            'Wall-clock local execution through screenshot capture; excludes model inference, network/tool transport and image viewing.',
            'Synthetic GTK fixture on private Xvnc 1280x800; legacy uses physical typing, native modes use clipboard paste.',
            'All modes include the same preparatory move to avoid the legacy same-position mousemove --sync stall.',
            'Seven rotated paired trials by default; one warmed persistent runtime; synthetic motor test has one trial.'
        ]),indent=2))
        print('Results:',a.output,flush=True)
    finally:
        session.stdin.close()
        try:session.wait(timeout=5)
        except subprocess.TimeoutExpired:session.kill();session.wait()
        session.stdout.close();session.stderr.close()

if __name__=='__main__': main()

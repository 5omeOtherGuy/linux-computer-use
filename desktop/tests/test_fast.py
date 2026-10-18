"""Native runner correctness tests, without any connection to the owner's display.

Run: python3 -m unittest discover -s tests -p test_fast.py -v
Set AGENT_DESKTOP_TEST_WS and AGENT_DESKTOP_TEST_STATE to reuse a synthetic test
fixture; otherwise one private workspace is created for this class and closed.
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / 'bin/agent-desktop')
sys.path.insert(0, str(ROOT / 'bin'))
from agent_desktop_fast import validate

class Failure(Exception):
    def __init__(self, msg, code=1):
        super().__init__(msg)
        self.code=code

class ValidationTest(unittest.TestCase):
    def test_rejects_malformed_actions_before_input(self):
        invalid = [None, {}, [{'action':[]}], [{'action':'unknown'}], [{'action':'key','keys':[]}],
                   [{'action':'paste','text':'bad\0text'}], [{'action':'click','x':True,'y':2}],
                   [{'action':'scroll','direction':'down','x':2}],
                   [{'action':'wait','seconds':float('nan')}], [{'action':'wait','seconds':10**400}],
                   [{'action':'focus'}], [{'action':'wait_window','name':'a','pid':12}],
                   [{'action':'wait_stable','region':'0,0,0,1'}],
                   [{'action':'paste','text':'ok','shell':'false'}]]
        for actions in invalid:
            with self.subTest(actions=actions), self.assertRaises(Failure): validate(actions,Failure)

    def test_linux_pid_range_and_zero_scroll_delay(self):
        validate([{'action':'wait_window','pid':3936190},
                  {'action':'scroll','direction':'down','interval':0}],Failure)

class NativeIntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp=tempfile.TemporaryDirectory(prefix='ad-fast-')
        cls.ws=os.environ.get('AGENT_DESKTOP_TEST_WS')
        cls.owned=not cls.ws
        cls.state=Path(os.environ.get('AGENT_DESKTOP_TEST_STATE',str(Path(cls.temp.name)/'ui.json')))
        if cls.owned:
            cls.ws='fast-test-'+str(os.getpid())
            r=subprocess.run([CLI,'new','--name',cls.ws,'--maximize','--json'],capture_output=True,text=True,
                             env=dict(os.environ,AGENT_DESKTOP_NOTIFY='0'),timeout=20)
            if r.returncode==3:
                cls.temp.cleanup(); raise unittest.SkipTest(r.stderr)
            if r.returncode: cls.temp.cleanup(); raise RuntimeError(r.stderr)
            try:
                cls.ad('run','--','python3',str(ROOT/'tests/fast_fixture.py'),str(cls.state))
                cls.ad('wait','--name','Agent Desktop Speed Fixture','--timeout','10')
            except BaseException:
                subprocess.run([CLI,'close',cls.ws],capture_output=True,timeout=20)
                cls.temp.cleanup(); raise

    @classmethod
    def tearDownClass(cls):
        if cls.owned: subprocess.run([CLI,'close',cls.ws],capture_output=True,timeout=20)
        cls.temp.cleanup()

    @classmethod
    def ad(cls,command,*args,check=True):
        r=subprocess.run([CLI,command,cls.ws,*args],capture_output=True,text=True,timeout=20)
        if check and r.returncode: raise AssertionError(r.stderr)
        return r

    def batch(self,actions,*flags,check=True):
        r=subprocess.run([CLI,'batch',self.ws,*flags],input=json.dumps(actions),
                         capture_output=True,text=True,timeout=20)
        if check and r.returncode: raise AssertionError(r.stderr)
        return json.loads(r.stdout) if r.returncode==0 else r

    def read(self): return json.loads(self.state.read_text())

    def wait_state(self,key,value):
        until=time.monotonic()+3
        while time.monotonic()<until:
            if self.read()[key]==value: return
            time.sleep(.01)
        self.assertEqual(self.read()[key],value)

    def test_unicode_and_literal_shell_text_pastes_exactly(self):
        text="Grüße 世界 🚀 $HOME $(false) `false` ' \" \\"
        result=self.batch([{'action':'click','x':200,'y':15},{'action':'key','keys':['ctrl+a']},
                           {'action':'paste','text':text},{'action':'key','keys':['Return']}])
        self.wait_state('entry',text)
        self.wait_state('submitted_entry',text)
        self.assertEqual(result['completed'],4)
        self.assertEqual(len(result['screenshots']),1)
        from PIL import Image
        with Image.open(result['screenshots'][0]) as im: self.assertEqual(im.size,(1280,800))

    def test_same_coordinate_moves_and_repeated_clicks_finish(self):
        before=self.read()['clicks']
        self.batch([{'action':'click','x':200,'y':780} for _ in range(8)])
        self.wait_state('clicks',before+8)

    def test_invalid_tail_has_no_side_effects(self):
        before=self.read()['clicks']
        r=self.batch([{'action':'click','x':200,'y':780},{'action':'key','keys':[]}],check=False)
        self.assertEqual(r.returncode,2)
        self.assertEqual(self.read()['clicks'],before)

    def test_partial_failure_does_not_run_later_actions(self):
        before=self.read()['clicks']
        r=self.batch([{'action':'click','x':200,'y':780},
                      {'action':'wait_window','name':'^this-window-does-not-exist$','timeout':.05},
                      {'action':'click','x':200,'y':780}],check=False)
        self.assertEqual(r.returncode,4)
        self.assertIn('after 1/3 actions',r.stderr)
        self.wait_state('clicks',before+1)

    def test_crop_scale_and_render_stability(self):
        r=self.batch([{'action':'wait_stable','region':'0,40,300,200','timeout':2}],
                     '--region','10,40,400,200','--scale','.5')
        from PIL import Image
        with Image.open(r['screenshots'][0]) as im: self.assertEqual(im.size,(200,100))

    def test_physical_ascii_typing_and_keyboard_order(self):
        self.batch([{'action':'click','x':200,'y':15},{'action':'key','keys':['ctrl+a']},
                    {'action':'type','text':'abc XYZ 123!? '},{'action':'key','keys':['End','BackSpace']}])
        self.wait_state('entry','abc XYZ 123!?')

    def test_drag_releases_button_and_scroll_is_accepted(self):
        self.batch([{'action':'click','x':200,'y':15},{'action':'key','keys':['ctrl+a']},
                    {'action':'paste','text':'drag selection test'},
                    {'action':'drag','x1':10,'y1':15,'x2':110,'y2':15},
                    {'action':'scroll','direction':'down','amount':3,'x':500,'y':400,'interval':0}])
        # A subsequent button activation verifies no grab/button remains held.
        before=self.read()['clicks']
        self.batch([{'action':'click','x':200,'y':780}])
        self.wait_state('clicks',before+1)

    def test_keyboard_modifiers_are_restored(self):
        self.ad('run','--wait','--','xdotool','keyup','Control_L','Control_R','Shift_L','Shift_R')
        self.ad('run','--wait','--','xdotool','keydown','Shift_L')
        try:
            self.batch([{'action':'click','x':200,'y':15},{'action':'key','keys':['ctrl+a']},
                        {'action':'type','text':'lowercase'}])
            self.wait_state('entry','lowercase')
            # Restored Shift applies to an external physical key event.
            self.ad('run','--wait','--','xdotool','key','a')
            self.wait_state('entry','lowercaseA')
        finally:
            self.ad('run','--wait','--','xdotool','keyup','Shift_L')

    def test_large_clipboard_transfer_preserves_multiline_unicode(self):
        text='Large Grüße 世界 🚀 line\n'*9000
        self.batch([{'action':'click','x':200,'y':100},{'action':'key','keys':['ctrl+a']},
                    {'action':'paste','text':text}])
        self.wait_state('text',text)
        self.batch([{'action':'key','keys':['ctrl+a','BackSpace']}])

    def test_persistent_session_recovers_after_validation_error(self):
        p=subprocess.Popen([CLI,'session',self.ws,'--region','0,40,300,200','--scale','.5'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE,text=True,bufsize=1)
        try:
            self.assertTrue(json.loads(p.stdout.readline())['ready'])
            def request(data):
                p.stdin.write(json.dumps(data)+'\n');p.stdin.flush()
                return json.loads(p.stdout.readline())
            self.assertEqual(request([{'action':[]}])['code'],2)
            shot=request([])['screenshots'][0]
            from PIL import Image
            with Image.open(shot) as im: self.assertEqual(im.size,(150,100))
            for i in range(40):
                text=f'Field {i}: Grüße 世界 🚀'
                r=request({'actions':[{'action':'click','x':200,'y':15},
                    {'action':'key','keys':['ctrl+a']},{'action':'paste','text':text}], 'screenshot':False})
                self.assertEqual(r['completed'],3)
                self.wait_state('entry',text)
            r=request([{'action':'screenshot','region':'0,40,300,200'}])
            self.assertEqual(len(r['screenshots']),1)
        finally:
            p.stdin.close()
            try:p.wait(timeout=5)
            except subprocess.TimeoutExpired:p.kill();p.wait()
            p.stdout.close();p.stderr.close()

if __name__=='__main__': unittest.main()

"""Native bridge invariants without GUI input or an AT-SPI/display connection."""
import errno
import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

MODULE = Path(__file__).resolve().parents[1] / 'bin/agent_desktop_native.py'
spec = importlib.util.spec_from_file_location('native_controls', MODULE)
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)
GEN='012345abcdef'
REF='n'+GEN+'_1'


class ValidationTests(unittest.TestCase):
    def test_malformed_late_operation_rejected(self):
        invalid = [None, {'op': 'key', 'ref': REF, 'keys': ['ctrl++']},
                   {'op': 'key', 'ref': REF, 'keys': ['ctrl+DefinitelyNotAKey']},
                   {'op': 'key', 'ref': REF, 'keys': ['ctrl+ctrl+a']},
                   {'op': 'key', 'ref': REF, 'keys': ['bogus+a']},
                   {'op': 'read', 'ref': 'n1'}, {'op': 'read', 'ref': 'nother_1'},
                   {'op': 'inspect', 'limit': True}, {'op': 'inspect', 'extra': True},
                   {'op': 'wait', 'app': 'Editor', 'query': 'x', 'timeout': 10**400}]
        for item in invalid:
            with self.subTest(item=item), self.assertRaises(ValueError):
                native.validate([{'op':'fill','ref':REF,'text':'first'}, item])

    def test_keysyms_without_display_and_modifier_normalization(self):
        self.assertEqual(native.key_chord('Control+shift+Left'),'ctrl+shift+Left')
        self.assertEqual(native.key_chord('alt+Return'),'alt+Return')
        self.assertEqual(native.key_chord('super+F12'),'super+F12')

    def test_aggregate_wait_budget(self):
        native.validate([{'op':'wait','app':'x','query':'y','timeout':30}]*2)
        with self.assertRaisesRegex(ValueError,'Total declared waits'):
            native.validate([{'op':'wait','app':'x','query':'y'}]*7)


class Accessible:
    def __init__(self, path, pid=1, count=0, children=None):
        self.path=path; self.pid=pid; self.name=path; self.childCount=count
        self.app=SimpleNamespace(bus_name=':1.'+str(pid)); self.children=children or []
        self.child_calls=0
    def get_process_id(self):return self.pid
    def getRoleName(self):return 'text'
    def getState(self):return SimpleNamespace(contains=lambda state:state=='showing')
    def getChildAtIndex(self,i):
        self.child_calls+=1
        return self.children[i] if self.children else Accessible('child'+str(i))


def controller():
    instance=native.Native.__new__(native.Native)
    instance.refs={};instance.identities={};instance.serial=0
    instance.generation=GEN;instance.workspace='workspace-one'
    instance.api=SimpleNamespace(STATE_SHOWING='showing',STATE_DEFUNCT='defunct')
    instance.pid_owned=lambda pid:pid==1
    instance.row=lambda a,pid=None:{'ref':instance.ref(a),'name':a.name,'role':a.getRoleName(),'pid':pid}
    return instance


class ReferenceTraversalTests(unittest.TestCase):
    def test_generation_and_workspace_refs_do_not_alias(self):
        one=controller(); obj=Accessible('entry')
        ref=one.ref(obj)
        self.assertEqual(ref,one.ref(obj))
        two=controller();two.generation='fedcba543210';two.ref(obj)
        with self.assertRaisesRegex(ValueError,'expired'):two.resolve(ref)
        self.assertNotEqual(ref,two.ref(obj))

    def test_foreign_descendant_never_read(self):
        instance=controller()
        foreign=Accessible('secret',pid=2)
        root=Accessible('root',count=1,children=[foreign]);instance.desktop=[root]
        result=instance.inspect()
        self.assertEqual([r['name'] for r in result['nodes']],['root'])
        self.assertNotIn(foreign.path,instance.identities)

    def test_observation_pid_lookups_once_per_bus_and_foreign_rejected(self):
        instance=controller()
        child=Accessible('same-bus');foreign=Accessible('foreign',pid=2)
        root=Accessible('root',count=2,children=[child,foreign]);instance.desktop=[root]
        for item in (root,child,foreign):item.get_process_id=Mock(return_value=item.pid)
        instance.pid_owned=Mock(side_effect=lambda pid:pid==1)
        result=instance.inspect()
        self.assertEqual([row['name'] for row in result['nodes']],['root','same-bus'])
        self.assertEqual([row['pid'] for row in result['nodes']],[1,1])
        root.get_process_id.assert_called_once();child.get_process_id.assert_not_called()
        foreign.get_process_id.assert_called_once()
        self.assertEqual(instance.pid_owned.call_count,2)
        instance.inspect()
        self.assertEqual(root.get_process_id.call_count,2)
        self.assertEqual(foreign.get_process_id.call_count,2)

    def test_resolve_rechecks_marker_after_observation(self):
        instance=controller();target=Accessible('entry');instance.desktop=[target]
        ref=instance.inspect()['nodes'][0]['ref']
        instance.pid_owned=Mock(return_value=False)
        with self.assertRaisesRegex(ValueError,'foreign'):instance.resolve(ref)
        instance.pid_owned.assert_called_once_with(1)

    def test_row_reuses_verified_pid(self):
        instance=controller();target=Accessible('entry')
        target.get_process_id=Mock(side_effect=AssertionError('Repeated PID request'))
        target.queryAction=Mock(side_effect=NotImplementedError)
        states=('editable','focusable','focused','active','modal','enabled','sensitive','selected','checked')
        instance.api=SimpleNamespace(**{'STATE_'+name.upper():name for name in states})
        instance.text=Mock(return_value={})
        self.assertEqual(native.Native.row(instance,target,pid=123)['pid'],123)
        target.get_process_id.assert_not_called()

    def test_limit_stops_sibling_remote_calls(self):
        instance=controller();root=Accessible('root',count=100000);instance.desktop=[root]
        result=instance.inspect(limit=1)
        self.assertTrue(result['truncated']);self.assertEqual(root.child_calls,0)

    def test_depth_stops_before_fetching_children(self):
        instance=controller();child=Accessible('child',count=100000)
        root=Accessible('root',count=1,children=[child]);instance.desktop=[root]
        result=instance.inspect(depth=1)
        self.assertTrue(result['truncated']);self.assertEqual(child.child_calls,0)


class ProtocolTests(unittest.TestCase):
    def test_frame_rejects_incomplete_oversized_and_extra_data(self):
        for data in (b'{',b'x'*20,b'{}\n{}\n'):
            with self.subTest(data=data):
                a,b=socket.socketpair()
                with a,b:
                    b.sendall(data);b.shutdown(socket.SHUT_WR)
                    with self.assertRaises((ValueError,ConnectionError)):
                        native.read_frame(a,10)

    def run_request(self, request, instance=None):
        instance=instance or controller();instance.run=Mock(return_value={'ok':True})
        server,client=socket.socketpair()
        errors=[]
        def serve():
            try:
                with server:native.handle_connection(server,instance)
            except BaseException as error:errors.append(error)
        with patch.dict(os.environ,DBUS_SESSION_BUS_ADDRESS='unix:path=/private/bus'):
            worker=threading.Thread(target=serve);worker.start()
            with client:
                hello=native.read_frame(client,4096)
                native.send_frame(client,request, native.MAX_REQUEST)
                result=native.read_frame(client,native.MAX_RESPONSE)
            worker.join(2)
        self.assertFalse(worker.is_alive());self.assertEqual(errors,[])
        return instance,hello,result

    def test_wrong_workspace_request_never_runs(self):
        instance,hello,result=self.run_request({'workspace':'wrong','generation':GEN,'actions':[]})
        self.assertEqual(hello['generation'],GEN);instance.run.assert_not_called()
        self.assertIn('mismatch',result['error'])

    def test_stale_ref_late_in_batch_prevents_all_input(self):
        request={'workspace':'workspace-one','generation':GEN,'actions':[
            {'op':'fill','ref':REF,'text':'first'},
            {'op':'read','ref':'nfedcba543210_1'}]}
        instance,_,result=self.run_request(request)
        instance.run.assert_not_called();self.assertIn('Expired',result['error'])

    def test_disconnect_does_not_escape_handler(self):
        instance=controller();server,client=socket.socketpair()
        client.close()
        with patch.dict(os.environ,DBUS_SESSION_BUS_ADDRESS='unix:path=/private/bus'),server:
            native.handle_connection(server,instance)

    def test_idle_connection_times_out_without_escaping(self):
        instance=controller()
        fake=Mock()
        fake.getsockopt.return_value=__import__('struct').pack('3i',os.getpid(),os.getuid(),os.getgid())
        fake.recv.side_effect=TimeoutError('idle client')
        with patch.dict(os.environ,DBUS_SESSION_BUS_ADDRESS='unix:path=/private/bus'):
            native.handle_connection(fake,instance)
        fake.settimeout.assert_called_with(5)
        result=json.loads(fake.sendall.call_args.args[0])
        self.assertEqual(result['completed'],0)
        self.assertIn('idle client',result['error'])

    def test_client_requires_matching_handshake_after_peer_check(self):
        import struct
        for workspace in ('workspace-one','wrong'):
            conn=Mock()
            conn.getsockopt.return_value=struct.pack('3i',os.getpid(),os.getuid(),os.getgid())
            greeting={'workspace':workspace,'generation':GEN,'bus':'unix:path=/private/bus','pid':os.getpid()}
            with patch.object(native.socket,'socket',return_value=conn), \
                 patch.object(native,'process_environment',return_value=[b'AGENT_DESKTOP_ID=workspace-one',b'DBUS_SESSION_BUS_ADDRESS=unix:path=/private/bus']), \
                 patch.object(native,'read_frame',return_value=greeting):
                if workspace=='wrong':
                    with self.assertRaisesRegex(ValueError,'handshake identity'):
                        native.connect_backend('/unused','workspace-one','unix:path=/private/bus')
                    conn.close.assert_called_once()
                else:
                    connection,hello=native.connect_backend('/unused','workspace-one','unix:path=/private/bus')
                    self.assertIs(connection,conn);self.assertEqual(hello['generation'],GEN)
                    conn.close.assert_not_called()
                conn.sendall.assert_not_called()

    def test_backend_peer_marker_checked_before_actions(self):
        with tempfile.TemporaryDirectory() as folder:
            path=str(Path(folder)/'native.sock')
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(path);listener.listen(1)
                def accept():
                    conn,_=listener.accept()
                    with conn:pass
                worker=threading.Thread(target=accept);worker.start()
                with patch.object(native,'process_environment',return_value=[b'AGENT_DESKTOP_ID=wrong']):
                    with self.assertRaisesRegex(ValueError,'different workspace'):
                        native.connect_backend(path,'workspace-one','unix:path=/private/bus')
                worker.join(2)


class ChooserFocusTests(unittest.TestCase):
    def chooser_tree(self):
        app=Mock(name='app');app.getRoleName.return_value='application'
        top=Mock();top.name='Open Files';top.parent=app
        top.getRoleName.return_value='file chooser';top.get_process_id.return_value=123
        filler=Mock();filler.parent=top;filler.getRoleName.return_value='filler'
        inner=Mock();inner.name='File Chooser Widget';inner.parent=filler
        inner.getRoleName.return_value='file chooser';inner.get_process_id.return_value=123
        frame=Mock();frame.name='Inner Frame';frame.parent=inner
        frame.getRoleName.return_value='frame';frame.get_process_id.return_value=123
        return top,inner,frame

    def test_nested_chooser_and_frame_activate_top_application_child(self):
        instance=controller();top,inner,frame=self.chooser_tree()
        windows='0x001 0 123 host Editor Main\n0x002 0 123 host Open Files\n'
        for target in (top,inner,frame):
            with self.subTest(target=target), \
                 patch.object(native.subprocess,'check_output',return_value=windows), \
                 patch.object(native.subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout='2\n')) as run:
                instance.activate(target)
                self.assertEqual(run.call_args_list[0].args[0],['wmctrl','-ia','0x002'])

    def test_chooser_container_never_requests_component_focus(self):
        instance=controller();top,inner,_=self.chooser_tree()
        instance.input=Mock();instance.activate=Mock()
        for target in (top,inner):
            instance.resolve=Mock(return_value=target)
            for action in ({'op':'focus','ref':REF},
                           {'op':'key','ref':REF,'keys':['ctrl+l']},
                           {'op':'paste','ref':REF,'text':'/private/file'}):
                with self.subTest(role=target.name,op=action['op']):
                    result=instance.run(action)
                    self.assertEqual(result['focused'],REF)
            target.queryComponent.assert_not_called()
        self.assertEqual(instance.input.key.call_count,2)
        self.assertEqual(instance.input.paste.call_count,2)

    def test_blank_title_alert_uses_unique_observed_geometry(self):
        instance=controller();instance.api.DESKTOP_COORDS=0
        app=Mock();app.getRoleName.return_value='application'
        alert=Mock();alert.name='Question';alert.parent=app
        alert.getRoleName.return_value='alert';alert.get_process_id.return_value=123
        alert.queryComponent.return_value.getExtents.return_value=(316,337,647,126)
        titles='0x001 0 123 host Editor Main\n0x002 0 123 host Open Files\n0x003 0 123 host \n'
        geometries=('0x001 0 123 0 0 1280 800 host Editor Main\n'
                    '0x002 0 123 0 0 1280 800 host Open Files\n'
                    '0x003 0 123 317 338 647 126 host \n')
        with patch.object(native.subprocess,'check_output',side_effect=[titles,geometries]), \
             patch.object(native.subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout='3\n')) as run:
            instance.activate(alert)
            self.assertEqual(run.call_args_list[0].args[0],['wmctrl','-ia','0x003'])
        instance.resolve=Mock(return_value=alert);instance.activate=Mock();instance.input=Mock()
        instance.run({'op':'key','ref':REF,'keys':['Escape']})
        alert.queryComponent.return_value.grabFocus.assert_not_called()

    def test_ambiguous_geometry_fails_without_activation(self):
        instance=controller();instance.api.DESKTOP_COORDS=0
        app=Mock();app.getRoleName.return_value='application'
        alert=Mock();alert.name='Question';alert.parent=app
        alert.getRoleName.return_value='alert';alert.get_process_id.return_value=123
        alert.queryComponent.return_value.getExtents.return_value=(316,337,647,126)
        titles='0x001 0 123 host \n0x002 0 123 host \n'
        geometry='0x001 0 123 316 337 647 126 host \n0x002 0 123 317 338 647 126 host \n'
        with patch.object(native.subprocess,'check_output',side_effect=[titles,geometry]), \
             patch.object(native.subprocess,'run') as run:
            with self.assertRaisesRegex(ValueError,'unique window'):instance.activate(alert)
            run.assert_not_called()

    def test_rows_expose_focusable_active_and_modal(self):
        instance=controller()
        states=('editable','focusable','focused','active','modal','enabled','sensitive','selected','checked')
        instance.api=SimpleNamespace(**{'STATE_'+name.upper():name for name in states})
        target=Accessible('chooser')
        target.getState=lambda:SimpleNamespace(contains=lambda state:state in ('active','modal'))
        target.queryAction=Mock(side_effect=NotImplementedError)
        instance.text=Mock(return_value={})
        row=native.Native.row(instance,target)
        self.assertEqual(row['state'],['active','modal'])
        target.getState=lambda:SimpleNamespace(contains=lambda state:state=='focusable')
        self.assertEqual(native.Native.row(instance,target)['state'],['focusable'])


class ClientRecoveryTests(unittest.TestCase):
    def workspace(self,folder):
        root=Path(folder);(root/'logs').mkdir()
        return SimpleNamespace(dir=root,files=root,state={'a11y':True,'id':'workspace-one'},
            env=lambda:{'DBUS_SESSION_BUS_ADDRESS':'unix:path=/private/bus'},touch=lambda:None)

    def test_no_retry_after_request_submission(self):
        with tempfile.TemporaryDirectory() as folder:
            workspace=self.workspace(folder)
            (workspace.dir/'native.sock').touch()
            conn=Mock();conn.__enter__=Mock(return_value=conn);conn.__exit__=Mock(return_value=False)
            hello={'generation':GEN}
            with patch.object(native,'connect_backend',return_value=(conn,hello)) as connect, \
                 patch.object(native,'send_frame') as send, \
                 patch.object(native,'read_frame',side_effect=TimeoutError('reply uncertain')), \
                 patch.object(native.subprocess,'Popen') as spawn:
                with self.assertRaises(TimeoutError):
                    native.run_cli(SimpleNamespace(actions='[]'),workspace,ValueError)
                self.assertEqual(connect.call_count,1);self.assertEqual(send.call_count,1)
                spawn.assert_not_called()

    def test_dead_socket_recovered_before_request(self):
        with tempfile.TemporaryDirectory() as folder:
            workspace=self.workspace(folder);path=workspace.dir/'native.sock'
            with socket.socket(socket.AF_UNIX) as dead:dead.bind(str(path))
            replacement=[]
            def spawn(*args,**kwargs):
                server=socket.socket(socket.AF_UNIX);server.bind(str(path));replacement.append(server)
                return SimpleNamespace(poll=lambda:None)
            conn=Mock();conn.__enter__=Mock(return_value=conn);conn.__exit__=Mock(return_value=False)
            with patch.object(native,'connect_backend',side_effect=[ConnectionRefusedError(errno.ECONNREFUSED,'dead'),(conn,{'generation':GEN})]) as connect, \
                 patch.object(native.subprocess,'Popen',side_effect=spawn) as start, \
                 patch.object(native,'send_frame') as send, \
                 patch.object(native,'read_frame',return_value={'completed':0,'results':[]}), \
                 patch('builtins.print'):
                try:native.run_cli(SimpleNamespace(actions='[]'),workspace,ValueError)
                finally:
                    for server in replacement:server.close()
                self.assertEqual(connect.call_count,2);self.assertEqual(start.call_count,1)
                self.assertEqual(send.call_count,1)


if __name__=='__main__':unittest.main()

class WorkspaceEnvironmentTests(unittest.TestCase):
    def test_private_accessibility_environment_drops_owner_bus(self):
        from importlib.machinery import SourceFileLoader
        from importlib.util import spec_from_loader, module_from_spec
        loader=SourceFileLoader('desktop_env_tests',str(Path(__file__).resolve().parents[1]/'bin/agent-desktop'))
        spec=spec_from_loader(loader.name,loader);desktop=module_from_spec(spec);loader.exec_module(desktop)
        ws=desktop.Workspace('environment-test')
        ws._state={'display':101,'id':'task-marker','a11y':True,'share_home':False,'audio':False}
        with patch.dict(os.environ,{'AT_SPI_BUS_ADDRESS':'unix:path=/foreign-bus','DISPLAY':':0'}):
            env=ws.env()
        self.assertEqual(env['DISPLAY'],':101')
        self.assertNotIn('AT_SPI_BUS_ADDRESS',env)
        self.assertEqual(env['NO_AT_BRIDGE'],'0')
        self.assertEqual(env['GTK_MODULES'],'atk-bridge')
        self.assertEqual(env['DBUS_SESSION_BUS_ADDRESS'],'unix:path='+str(ws.dir/'bus'))
        ws._state['a11y']=False
        self.assertEqual(ws.env()['NO_AT_BRIDGE'],'1')

class LocalFlowTests(unittest.TestCase):
    def test_aliases_must_be_defined_once_before_any_input(self):
        valid=[{'op':'find','app':'editor','role':'text','as':'edit'},
               {'op':'fill','ref':'$edit','text':'hello'},
               {'op':'find','app':'editor','role':'push button','scope':'$edit','as':'save','name':'Save'}]
        native.validate(valid)
        for bad in ([valid[1],valid[0]], [valid[0],valid[0]],
                    [{'op':'find','app':'editor','role':'text','as':'$edit'}],
                    [{'op':'find','app':'','role':'text','as':'edit'}]):
            with self.assertRaises(ValueError):native.validate(bad)

    def test_request_local_bindings_resolve_refs_and_scopes(self):
        instance=controller()
        instance.run=Mock(side_effect=[{'ref':REF},{'focused':REF},{'ref':REF}])
        done=[]
        native.execute([{'op':'find','app':'editor','role':'frame','as':'win'},
                        {'op':'focus','ref':'$win'},
                        {'op':'find','app':'editor','role':'text','scope':'$win','as':'edit'}],instance,done)
        self.assertEqual(instance.run.call_args_list[1].args[0]['ref'],REF)
        self.assertEqual(instance.run.call_args_list[2].args[0]['scope'],REF)
        self.assertEqual(len(done),3)
        instance.run=Mock(side_effect=TimeoutError('no match'));done=[]
        with self.assertRaises(TimeoutError):native.execute([{'op':'find','as':'w'},{'op':'focus','ref':'$w'}],instance,done)
        self.assertEqual(done,[]);self.assertEqual(instance.run.call_count,1)

    def test_find_rejects_ambiguous_or_incomplete_observations(self):
        instance=controller()
        for snapshot in ({'nodes':[{'ref':REF},{'ref':REF}],'truncated':False},
                         {'nodes':[{'ref':REF}],'truncated':True}):
            instance.inspect=Mock(return_value=snapshot)
            with self.assertRaises(ValueError):instance.run({'op':'find','app':'editor','role':'text','as':'a'})

    def test_failed_child_read_marks_lookup_incomplete(self):
        instance=controller();broken=Accessible('broken')
        broken.getState=Mock(side_effect=RuntimeError('gone'))
        root=Accessible('root',count=2,children=[Accessible('match'),broken]);instance.desktop=[root]
        result=instance.inspect(selector={'role':'text','name':'match'})
        self.assertEqual(len(result['nodes']),1);self.assertTrue(result['truncated'])

    def test_exact_selector_can_find_unnamed_controls(self):
        instance=controller();target=Accessible('');target.getRoleName=lambda:'push button'
        instance.desktop=[target]
        result=instance.run({'op':'find','app':'1','role':'push button','as':'button'})
        self.assertEqual(result['name'],'');self.assertEqual(result['role'],'push button')

    def test_scope_cannot_silently_override_requested_app(self):
        instance=controller();target=Accessible('Other');target.getRoleName=lambda:'application'
        instance.resolve=Mock(return_value=target)
        with self.assertRaisesRegex(ValueError,'requested app'):
            instance.run({'op':'find','app':'Editor','scope':REF,'role':'text','as':'a'})

class SelectionTests(unittest.TestCase):
    def test_nested_cell_selects_via_nearest_supported_parent(self):
        instance=controller();instance.activate=Mock();instance.owned=Mock(return_value=True)
        leaf=Mock();row=Mock();table=Mock()
        leaf.parent=row;row.parent=table
        row.querySelection.side_effect=NotImplementedError
        row.getIndexInParent.return_value=4
        selection=table.querySelection.return_value
        selection.clearSelection.return_value=True;selection.selectChild.return_value=True
        selection.isChildSelected.return_value=True
        instance.resolve=Mock(return_value=leaf);instance.ref=Mock(return_value='container-ref')
        result=instance.run({'op':'select','ref':REF})
        selection.selectChild.assert_called_once_with(4)
        selection.isChildSelected.assert_called_once_with(4)
        self.assertEqual(result['selected'],REF)
        self.assertEqual(result['selection_container'],'container-ref')

    def test_refused_selection_stops_without_shortcuts(self):
        instance=controller();instance.activate=Mock();instance.owned=Mock(return_value=True)
        leaf=Mock();leaf.getIndexInParent.return_value=1
        leaf.parent.querySelection.return_value.clearSelection.return_value=False
        instance.resolve=Mock(return_value=leaf);instance.input=Mock()
        with self.assertRaisesRegex(RuntimeError,'inspect before continuing'):
            instance.run({'op':'select','ref':REF})
        instance.input.key.assert_not_called()

class PreciseLookupTests(unittest.TestCase):
    def test_top_lookup_does_not_traverse_window_contents(self):
        instance=controller();app=Accessible('editor',count=1);app.getRoleName=lambda:'application'
        window=Accessible('Save As…',count=100000);window.getRoleName=lambda:'file chooser';window.parent=app
        app.children=[window];instance.desktop=[app]
        result=instance.run({'op':'find','app':'editor','role':'file chooser','top':True,'as':'save'})
        self.assertEqual(result['name'],'Save As…');self.assertEqual(window.child_calls,0)

    def test_required_states_are_validated_and_matched(self):
        with self.assertRaises(ValueError):native.validate([{'op':'find','app':'x','role':'text','as':'x','states':['invented']}])
        instance=controller();target=Accessible('entry');instance.desktop=[target]
        instance.row=lambda a,pid=None:{'ref':instance.ref(a),'role':'text','name':'entry','state':['editable','focused']}
        snapshot=instance.inspect(selector={'role':'text','states':['editable','focused']})
        self.assertEqual(len(snapshot['nodes']),1)
        self.assertEqual(instance.inspect(selector={'role':'text','states':['selected']})['nodes'],[])

    def test_table_fallback_uses_observed_index_and_verified_row(self):
        instance=controller();instance.activate=Mock();instance.owned=Mock(return_value=True)
        leaf=Mock();leaf.getIndexInParent.return_value=7
        selection=leaf.parent.querySelection.return_value
        selection.clearSelection.return_value=True;selection.selectChild.return_value=False
        table=leaf.parent.queryTable.return_value
        table.getRowAtIndex.return_value=2;table.nRows=4
        table.addRowSelection.return_value=True;table.getSelectedRows.return_value=[2]
        instance.resolve=Mock(return_value=leaf);instance.ref=Mock(return_value=REF)
        self.assertEqual(instance.run({'op':'select','ref':REF})['selected'],REF)
        table.getRowAtIndex.assert_called_once_with(7);table.addRowSelection.assert_called_once_with(2)
        table.getSelectedRows.return_value=[1,2]
        with self.assertRaisesRegex(RuntimeError,'not verified'):instance.run({'op':'select','ref':REF})

class LookupCompletenessTests(unittest.TestCase):
    def test_application_root_need_not_be_showing(self):
        instance=controller();app=Accessible('editor',count=1)
        app.getRoleName=lambda:'application';app.getState=lambda:SimpleNamespace(contains=lambda _:False)
        window=Accessible('Main');window.getRoleName=lambda:'frame';window.parent=app
        app.children=[window];instance.desktop=[app]
        self.assertEqual(instance.run({'op':'find','app':'editor','role':'frame','top':True,'as':'w'})['name'],'Main')

    def test_none_child_marks_uniqueness_incomplete(self):
        instance=controller();root=Accessible('root',count=2,children=[Accessible('match'),None]);instance.desktop=[root]
        result=instance.inspect(selector={'role':'text','name':'match'})
        self.assertEqual(len(result['nodes']),1);self.assertTrue(result['truncated'])

    def test_hidden_scope_is_not_returned_as_showing_match(self):
        instance=controller();target=Accessible('hidden');target.getState=lambda:SimpleNamespace(contains=lambda _:False)
        instance.resolve=Mock(return_value=target)
        result=instance.inspect(scope=REF,selector={'role':'text'})
        self.assertEqual(result['nodes'],[])

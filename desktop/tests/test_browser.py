"""Browser controls tests. No display, browser input, or workspace creation required."""
import asyncio
import importlib.util
import json
import io
import shutil
import subprocess
import tempfile
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

MODULE = Path(__file__).resolve().parents[1] / 'bin/agent_desktop_browser.py'
spec = importlib.util.spec_from_file_location('browser_controls', MODULE)
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class ValidationTests(unittest.IsolatedAsyncioTestCase):
    async def test_entire_batch_rejected_before_connecting(self):
        invalid = [
            None, {}, {'op': []}, {'op': 'unknown'}, {'op': 'click'},
            {'op': 'fill', 'ref': 'b1'}, {'op': 'click', 'ref': 'b0'},
            {'op': 'read', 'ref': 'b-1'}, {'op': 'read', 'ref': 'b2147483648'}, {'op': 'context', 'ref': 1},
            {'op': 'inspect', 'scope': 'x1'}, {'op': 'inspect', 'limit': True},
            {'op': 'inspect', 'limit': 0}, {'op': 'inspect', 'limit': 1001},
            {'op': 'inspect', 'contexts': 1}, {'op': 'wait', 'query': 'x', 'contexts': 'yes'},
            {'op': 'inspect', 'role': ''}, {'op': 'inspect', 'role': ['link']},
            {'op': 'inspect', 'main': 1}, {'op': 'inspect', 'scope': 'b1', 'main': True},
            {'op': 'fill', 'ref': 'b1', 'text': None},
            {'op': 'fill', 'ref': 'b1', 'text': '\0'},
            {'op': 'wait', 'query': ''}, {'op': 'wait', 'query': None},
            {'op': 'wait', 'query': 'x', 'timeout': float('nan')},
            {'op': 'wait', 'query': 'x', 'timeout': 10**400},
            {'op': 'wait', 'query': 'x', 'timeout': True},
            {'op': 'wait', 'query': 'x', 'timeout': 0},
            {'op': 'wait', 'query': 'x', 'timeout': 61},
            {'op': 'goto', 'url': 'javascript:alert(1)'},
            {'op': 'goto', 'url': 'https://'},
            {'op': 'wait_url', 'url': '', 'contains': True},
            {'op': 'wait_url', 'url': 'https://example.test', 'contains': 'yes'},
            {'op': 'press', 'keys': 'Return'}, {'op': 'press', 'keys': []},
            {'op': 'press', 'keys': ['ctrl+']},
            {'op': 'press', 'keys': ['unknown+a']},
            {'op': 'press', 'keys': [None]},
            {'op': 'press', 'keys': ['Return'], 'unexpected': 'bad'},
        ]
        with patch.object(bridge, 'Browser') as browser:
            for item in invalid:
                with self.subTest(item=item), self.assertRaises(ValueError):
                    await bridge.execute([{'op': 'press', 'keys': ['Return']}, item], '/unused')
            browser.assert_not_called()

    def test_valid_schema_and_bounds(self):
        bridge.validate([
            {'op': 'inspect', 'main': True, 'limit': 1000},
            {'op': 'inspect', 'scope': 'b123', 'query': 'link'},
            {'op': 'fill', 'ref': 'b3', 'text': ''},
            {'op': 'goto', 'url': 'https://example.test/path', 'timeout': 60},
            {'op': 'wait_url', 'url': '?q=', 'contains': True},
            {'op': 'press', 'keys': ['ctrl+shift+a', 'alt+Left']},
        ])
        self.assertEqual(bridge.parse_key('ctrl+shift+a'), ('a', 'KeyA', 65, 10))
        self.assertEqual(bridge.parse_key('alt+Left'), ('ArrowLeft', 'ArrowLeft', 37, 1))

    def test_shared_home_rejected_before_start_or_attach(self):
        ws = SimpleNamespace(files=Path('/unused'), state={'share_home': True}, touch=lambda: None)
        for start in (True, False):
            args = SimpleNamespace(start=start, actions='[]', url=None)
            with self.subTest(start=start), self.assertRaisesRegex(ValueError, 'private workspace'):
                bridge.run_cli(args, ws, ValueError, lambda _: self.fail('Must not launch'))

    def test_cli_rejects_ignored_arguments_and_invalid_start_url(self):
        ws = SimpleNamespace(files=Path('/unused'), state={'share_home': False}, touch=lambda: None)
        cases = [
            (SimpleNamespace(start=True, actions='[]', url=None), 'does not accept actions'),
            (SimpleNamespace(start=False, actions='[]', url='https://example.test'), 'requires --start'),
            (SimpleNamespace(start=True, actions=None, url='javascript:alert(1)'), 'absolute HTTP'),
            (SimpleNamespace(start=True, actions=None, url=''), 'must not be empty'),
        ]
        for args, message in cases:
            with self.subTest(args=args), self.assertRaisesRegex(ValueError, message):
                bridge.run_cli(args, ws, ValueError, lambda _: self.fail('Must not launch'))


class NavigationTests(unittest.IsolatedAsyncioTestCase):
    async def test_redirect_waits_for_new_loader_and_returns_final_url(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock(side_effect=[
            {'frameId': 'frame', 'loaderId': 'new'},
            {'frameTree': {'frame': {'id': 'frame', 'loaderId': 'old'}}},
            {'frameTree': {'frame': {'id': 'frame', 'loaderId': 'new'}}},
        ])
        browser.eval = AsyncMock(return_value={'url': 'https://example.test/final', 'ready': 'complete'})
        with patch.object(bridge.asyncio, 'sleep', new=AsyncMock()):
            result = await browser.run({'op': 'goto', 'url': 'https://example.test/redirect'})
        self.assertEqual(result['url'], 'https://example.test/final')
        self.assertEqual(browser.eval.await_count, 1)
        self.assertEqual(browser.call.await_count, 3)

    async def test_navigation_error_is_immediate(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock(return_value={'frameId': 'f', 'errorText': 'net::ERR_NAME_NOT_RESOLVED'})
        browser.eval = AsyncMock()
        with self.assertRaisesRegex(RuntimeError, 'ERR_NAME_NOT_RESOLVED'):
            await browser.run({'op': 'goto', 'url': 'https://nonexistent.test'})
        browser.eval.assert_not_called()

    async def test_same_document_navigation_has_no_loader_id(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock(side_effect=[{'frameId': 'f'}, {'frameTree': {'frame': {'id': 'f', 'loaderId': 'unchanged'}}}])
        browser.eval = AsyncMock(return_value={'url': 'https://example.test/#section', 'ready': 'complete'})
        result = await browser.run({'op': 'goto', 'url': 'https://example.test/#section'})
        self.assertTrue(result['url'].endswith('#section'))

    async def test_transient_destroyed_context_retried(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock(side_effect=[{'frameId': 'f', 'loaderId': 'new'}] +
            [{'frameTree': {'frame': {'id': 'f', 'loaderId': 'new'}}}] * 2)
        browser.eval = AsyncMock(side_effect=[RuntimeError('Execution context was destroyed'),
            {'url': 'https://example.test/final', 'ready': 'complete'}])
        with patch.object(bridge.asyncio, 'sleep', new=AsyncMock()):
            result = await browser.run({'op': 'goto', 'url': 'https://example.test/redirect'})
        self.assertEqual(result['ready'], 'complete')


class ObservationTests(unittest.IsolatedAsyncioTestCase):
    async def test_main_scope_and_root_fallback(self):
        tree = [
            {'nodeId': 'root', 'backendDOMNodeId': 1, 'role': {'value': 'RootWebArea'}, 'name': {'value': 'Page'}, 'childIds': ['nav', 'main']},
            {'nodeId': 'nav', 'backendDOMNodeId': 2, 'role': {'value': 'navigation'}, 'name': {'value': 'Header'}},
            {'nodeId': 'main', 'backendDOMNodeId': 3, 'role': {'value': 'main'}, 'name': {'value': 'Content'}, 'childIds': ['link']},
            {'nodeId': 'link', 'backendDOMNodeId': 4, 'role': {'value': 'link'}, 'name': {'value': 'Issue'}, 'parentId': 'main'},
        ]
        browser = bridge.Browser('/unused')
        browser.eval = AsyncMock(return_value={'url': 'https://example.test', 'title': 'Page', 'ready': 'complete'})
        browser.call = AsyncMock(return_value={'nodes': tree})
        result = await browser.run({'op': 'inspect', 'main': True})
        self.assertEqual([n['ref'] for n in result['nodes']], ['b3', 'b4'])
        self.assertEqual(result['nodes'][1]['parent'], 'b3')
        tree[2]['role']['value'] = 'region'
        result = await browser.run({'op': 'inspect', 'main': True})
        self.assertEqual(len(result['nodes']), 4)


class ContextObservationTests(unittest.IsolatedAsyncioTestCase):
    async def test_filtered_contexts_only_resolve_returned_matches(self):
        browser = bridge.Browser('/unused')
        tree = [{'nodeId': 'root', 'role': {'value': 'RootWebArea'},
                 'childIds': ['text', 'first', 'second']}]
        tree += [dict(nodeId=ident, backendDOMNodeId=number,
                      role={'value': role}, name={'value': 'Result'})
                 for ident, number, role in [('text', 1, 'StaticText'),
                                             ('first', 2, 'link'), ('second', 3, 'link')]]
        browser.eval = AsyncMock(return_value={'url': 'https://example.test'})
        browser.call = AsyncMock(return_value={'nodes': tree})
        context = {'links': [{'text': 'Complete result title', 'url': 'https://example.test/1'}],
                   'times': [{'text': '2h ago', 'datetime': '2026-10-15T12:00:00Z'}]}
        browser.context = AsyncMock(return_value=context)
        result = await browser.run({'op': 'inspect', 'query': 'Result', 'role': 'link',
                                    'limit': 1, 'contexts': True})
        self.assertEqual(result['total_matches'], 2)
        self.assertTrue(result['truncated'])
        self.assertEqual(result['nodes'][0]['context'], context)
        browser.context.assert_awaited_once_with('b2', strict=False)

    async def test_wait_returns_context_in_matching_observation(self):
        browser = bridge.Browser('/unused')
        expected = {'nodes': [{'ref': 'b2', 'context': None}]}
        browser.inspect = AsyncMock(return_value=expected)
        result = await browser.run({'op': 'wait', 'query': 'Result', 'role': 'link',
                                    'main': True, 'contexts': True, 'limit': 3})
        self.assertEqual(result, expected)
        browser.inspect.assert_awaited_once_with('Result', 3, main=True, contexts=True, role='link')


class IdentityTests(unittest.IsolatedAsyncioTestCase):
    async def test_stale_port_browser_identity_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            profile = Path(folder)
            (profile / 'DevToolsActivePort').write_text('12345\n/devtools/browser/expected\n')
            with patch.object(bridge.urllib.request, 'urlopen', return_value=io.StringIO(json.dumps({
                'webSocketDebuggerUrl': 'ws://127.0.0.1:12345/devtools/browser/other'}))), \
                 patch.object(bridge.websockets, 'connect', new=AsyncMock()) as connect:
                with self.assertRaisesRegex(ValueError, 'identity differs'):
                    await bridge.Browser(profile).__aenter__()
                connect.assert_not_called()

    async def test_pinned_tab_wins_over_new_first_tab(self):
        with tempfile.TemporaryDirectory() as folder:
            profile = Path(folder)
            (profile / 'DevToolsActivePort').write_text('12345\n/devtools/browser/expected\n')
            (profile / 'agent-desktop-target').write_text('selected')
            version = {'webSocketDebuggerUrl': 'ws://127.0.0.1:12345/devtools/browser/expected'}
            pages = [{'id': ident, 'type': 'page', 'webSocketDebuggerUrl': 'ws://127.0.0.1:12345/devtools/page/' + ident}
                     for ident in ('new-tab', 'selected')]
            with patch.object(bridge.urllib.request, 'urlopen', side_effect=[io.StringIO(json.dumps(version)), io.StringIO(json.dumps(pages))]), \
                 patch.object(bridge.websockets, 'connect', new=AsyncMock()) as connect:
                async with bridge.Browser(profile):
                    pass
                self.assertEqual(connect.await_args.args[0], 'ws://127.0.0.1:12345/devtools/page/selected')


class ShadowTimeTests(unittest.IsolatedAsyncioTestCase):
    @unittest.skipUnless(shutil.which('node'), 'Node is needed only to execute the isolated DOM stub')
    async def test_context_reads_open_shadow_time_text(self):
        browser = bridge.Browser('/unused')
        browser.element = AsyncMock()
        await browser.run({'op': 'context', 'ref': 'b1'})
        function = browser.element.await_args.args[1]
        # This is a local DOM stub, not browser input or an external connection.
        script = """
const time = {innerText:'', shadowRoot:{textContent:'8h ago'}, textContent:'',
              getAttribute:n=>n==='datetime'?'2026-10-15T11:47:07Z':null};
const row = {innerText:'Issue Updated',querySelectorAll:q=>q==='a[href]'?[]:[time]};
const element = {nodeType:1,closest:()=>row};
console.log(JSON.stringify((FUNCTION).call(element)));
""".replace('FUNCTION', function)
        result = subprocess.run([shutil.which('node'), '-e', script], text=True, capture_output=True, check=True, timeout=5)
        self.assertEqual(json.loads(result.stdout)['times'][0]['text'], '8h ago')
        script = "const f=(FUNCTION); console.log(JSON.stringify(f.call({nodeType:1,closest:()=>null},false))); try { f.call({nodeType:1,closest:()=>null},true); process.exit(2); } catch(e) {}".replace('FUNCTION', function)
        result = subprocess.run([shutil.which('node'), '-e', script], text=True, capture_output=True, check=True, timeout=5)
        self.assertIsNone(json.loads(result.stdout))


if __name__ == '__main__':
    unittest.main()

class DesktopStartupTests(unittest.TestCase):
    def test_signal_uses_dedicated_private_profile_and_ready_page(self):
        with tempfile.TemporaryDirectory() as directory:
            ws = SimpleNamespace(name='test', files=Path(directory), state={'share_home': False}, touch=lambda: None)
            args = SimpleNamespace(start=True, actions=None, url=None, app='signal')
            launched=[]
            def launch(a):
                launched.append(a.cmd)
                profile=Path(directory)/'signal-control'
                profile.mkdir()
                (profile/'DevToolsActivePort').write_text('1234\n/devtools/browser/test\n')
            with patch('shutil.which', return_value='/opt/Signal/signal-desktop'), patch.object(bridge,'Browser') as browser:
                browser.return_value.__aenter__=AsyncMock()
                browser.return_value.__aexit__=AsyncMock()
                bridge.run_cli(args,ws,ValueError,launch)
                browser.assert_called_once_with(Path(directory)/'signal-control')
            self.assertIn('--remote-debugging-address=127.0.0.1',launched[0])
            self.assertIn('--user-data-dir='+str(Path(directory)/'signal-control'),launched[0])
            self.assertNotIn('about:blank',launched[0])
            self.assertIn('--force-renderer-accessibility',launched[0])

class ObservationRaceTests(unittest.IsolatedAsyncioTestCase):
    async def test_wait_retries_observation_during_navigation_only(self):
        browser=bridge.Browser('/unused')
        expected={'nodes':[{'role':'heading','name':'ready'}]}
        browser.inspect=AsyncMock(side_effect=[RuntimeError('Inspected target navigated or closed'),expected])
        with patch.object(bridge.asyncio,'sleep',new=AsyncMock()):
            result=await browser.run({'op':'wait','query':'ready'})
        self.assertEqual(result,expected)
        self.assertEqual(browser.inspect.await_count,2)

    async def test_wait_preserves_nontransient_errors(self):
        browser=bridge.Browser('/unused')
        browser.inspect=AsyncMock(side_effect=RuntimeError('Permission denied'))
        with self.assertRaisesRegex(RuntimeError,'Permission denied'):
            await browser.run({'op':'wait','query':'ready'})
        self.assertEqual(browser.inspect.await_count,1)

class InputFocusTests(unittest.IsolatedAsyncioTestCase):
    async def test_fill_raises_page_focuses_selects_replaces_and_verifies(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock()
        browser.element = AsyncMock(side_effect=[{'tag': 'INPUT'}, True, 'replacement'])
        result = await browser.run({'op': 'fill', 'ref': 'b3', 'text': 'replacement'})
        self.assertEqual(result, {'filled': 'b3'})
        calls = browser.call.await_args_list
        self.assertEqual([c.args[0] for c in calls], ['Page.bringToFront', 'DOM.focus',
                         'Input.dispatchKeyEvent', 'Input.dispatchKeyEvent', 'Input.insertText'])
        self.assertEqual(calls[1].kwargs, {'backendNodeId': 3})
        self.assertEqual(calls[2].kwargs['commands'], ['selectAll'])
        self.assertNotIn('commands', calls[3].kwargs)
        self.assertEqual(calls[4].kwargs, {'text': 'replacement'})
        self.assertIn('this.value', browser.element.await_args.args[1])

    async def test_fill_readback_mismatch_stops_without_reinsertion(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock()
        browser.element = AsyncMock(side_effect=[{'tag': 'INPUT'}, True, 'oldreplacement'])
        with self.assertRaisesRegex(RuntimeError, 'readback differs'):
            await browser.run({'op': 'fill', 'ref': 'b3', 'text': 'replacement'})
        self.assertEqual(sum(c.args[0] == 'Input.insertText' for c in browser.call.await_args_list), 1)

    async def test_failed_editable_focus_sends_no_selection_or_text(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock()
        browser.element = AsyncMock(side_effect=[{'tag': 'TEXTAREA'}, False])
        with self.assertRaisesRegex(RuntimeError, 'did not receive focus'):
            await browser.run({'op': 'fill', 'ref': 'b3', 'text': 'replacement'})
        self.assertEqual([c.args[0] for c in browser.call.await_args_list], ['Page.bringToFront', 'DOM.focus'])

    async def test_press_and_click_raise_pinned_page_before_input(self):
        for operation in ({'op': 'press', 'keys': ['Return']}, {'op': 'click', 'ref': 'b3'}):
            browser = bridge.Browser('/unused')
            browser.call = AsyncMock()
            browser.element = AsyncMock(return_value={'x': 4, 'y': 5})
            await browser.run(operation)
            self.assertEqual(browser.call.await_args_list[0].args[0], 'Page.bringToFront')

    async def test_select_all_command_only_applies_to_control_a(self):
        browser = bridge.Browser('/unused')
        browser.call = AsyncMock()
        for key in ('a', 'ctrl+shift+a', 'alt+a', 'Return'):
            await browser.press(key)
        self.assertTrue(all('commands' not in call.kwargs for call in browser.call.await_args_list))


class ProtocolTimeoutTests(unittest.IsolatedAsyncioTestCase):
    async def test_timeout_names_method_and_never_resends(self):
        browser = bridge.Browser('/unused')
        browser.seq = 0
        browser.ws = SimpleNamespace(send=AsyncMock(), recv=AsyncMock(side_effect=asyncio.TimeoutError))
        with self.assertRaisesRegex(TimeoutError, 'CDP Input.insertText response timed out'):
            await browser.call('Input.insertText', text='once')
        browser.ws.send.assert_awaited_once()
        browser.ws.recv.assert_awaited_once()
        sent = json.loads(browser.ws.send.await_args.args[0])
        self.assertEqual(sent, {'id': 1, 'method': 'Input.insertText', 'params': {'text': 'once'}})


class TextNodeContextTests(unittest.IsolatedAsyncioTestCase):
    @unittest.skipUnless(shutil.which('node'), 'Node is needed only to execute the isolated DOM stub')
    async def test_context_promotes_text_node_and_handles_detached_text(self):
        browser = bridge.Browser('/unused')
        browser.element = AsyncMock()
        await browser.context('b1')
        function = browser.element.await_args.args[1]
        script = """
const f=(FUNCTION);
const row={innerText:'Row title',querySelectorAll:()=>[]};
const parent={nodeType:1,closest:()=>row};
const text={nodeType:3,parentElement:parent};
const detached={nodeType:3,parentElement:null};
const result=f.call(text,true);
if(result.text!=='Row title')throw Error('Text node row missing');
if(f.call(detached,false)!==null)throw Error('Detached text should have no context');
let threw=false;try{f.call(detached,true)}catch(e){threw=/No list or table row ancestor/.test(e.message)}
if(!threw)throw Error('Strict context should explain missing row');
console.log(JSON.stringify(result));
""".replace('FUNCTION', function)
        result = subprocess.run([shutil.which('node'), '-e', script], text=True, capture_output=True, check=True, timeout=5)
        self.assertEqual(json.loads(result.stdout)['text'], 'Row title')

class BindingTests(unittest.IsolatedAsyncioTestCase):
    async def test_binding_validation_precedes_attachment(self):
        find={'op':'find','role':'combobox','name':'Search','as':'search'}
        bad_batches=[ [{'op':'fill','ref':'$search','text':'x'},find], [find,find],
                      [dict(find,**{'as':'bad-name'})], [dict(find,scope='$search')]]
        with patch.object(bridge,'Browser') as browser:
            for actions in bad_batches:
                with self.assertRaises(ValueError):await bridge.execute(actions,'/unused')
            browser.assert_not_called()

    async def test_find_requires_unique_complete_snapshot(self):
        browser=bridge.Browser('/unused')
        for nodes,truncated in [([{'ref':'b1','name':'Search'},{'ref':'b2','name':'Search'}],False),([{'ref':'b1','name':'Search'}],True)]:
            browser.inspect=AsyncMock(return_value={'nodes':nodes,'truncated':truncated})
            with self.assertRaises(ValueError):await browser.run({'op':'find','role':'combobox','as':'s'})
        browser.inspect=AsyncMock(return_value={'nodes':[{'ref':'b1','name':'Search'},{'ref':'b2','name':'Other'}],'truncated':False})
        self.assertEqual((await browser.run({'op':'find','role':'combobox','name':'Search','as':'s'}))['ref'],'b1')

    async def test_local_binding_resolves_ref_and_scope_and_stops_on_failure(self):
        browser=bridge.Browser('/unused');browser.run=AsyncMock(side_effect=[{'ref':'b7'},{'filled':'b7'},{'nodes':[]}])
        class Context:
            async def __aenter__(self):return browser
            async def __aexit__(self,*args):pass
        actions=[{'op':'find','role':'combobox','as':'s'}, {'op':'fill','ref':'$s','text':'x'}, {'op':'inspect','scope':'$s'}]
        with patch.object(bridge,'Browser',return_value=Context()):await bridge.execute(actions,'/unused')
        self.assertEqual(browser.run.await_args_list[1].args[0]['ref'],'b7')
        self.assertEqual(browser.run.await_args_list[2].args[0]['scope'],'b7')
        browser.run=AsyncMock(side_effect=ValueError('Ambiguous'));done=[]
        with patch.object(bridge,'Browser',return_value=Context()),self.assertRaises(ValueError):await bridge.execute(actions,'/unused',done)
        self.assertEqual(done,[]);self.assertEqual(browser.run.await_count,1)

class ExactLookupTests(unittest.IsolatedAsyncioTestCase):
    def browser(self,names):
        browser=bridge.Browser('/unused')
        tree=[{'nodeId':'root','role':{'value':'RootWebArea'},'childIds':[str(i)for i in range(len(names))]}]
        tree += [{'nodeId':str(i),'backendDOMNodeId':i+1,'role':{'value':'button'},'name':{'value':name}}for i,name in enumerate(names)]
        browser.eval=AsyncMock(return_value={'url':'https://example.test'})
        browser.call=AsyncMock(return_value={'nodes':tree})
        return browser

    async def test_unnamed_controls_count_toward_role_uniqueness(self):
        browser=self.browser(['Save',''])
        with self.assertRaisesRegex(ValueError,'Ambiguous'):
            await browser.run({'op':'find','role':'button','as':'button'})
        row=await browser.run({'op':'find','role':'button','name':'','as':'button'})
        self.assertEqual(row['ref'],'b2')

    async def test_long_display_prefix_cannot_satisfy_exact_name(self):
        prefix='a'*400;browser=self.browser([prefix+'suffix'])
        snapshot=await browser.inspect(role='button',lookup=True)
        self.assertTrue(snapshot['nodes'][0]['name_truncated'])
        with self.assertRaises(TimeoutError):
            await browser.run({'op':'find','role':'button','name':prefix,'as':'button','timeout':0.00001})
        browser=self.browser([prefix])
        self.assertEqual((await browser.run({'op':'find','role':'button','name':prefix,'as':'button'}))['ref'],'b1')

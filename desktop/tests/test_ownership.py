"""Close authorization and private-file regressions; no X server, GUI, or existing workspace touched."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1] / 'bin/agent-desktop'
loader = importlib.machinery.SourceFileLoader('desktop_ownership', str(CLI))
spec = importlib.util.spec_from_loader(loader.name, loader)
ad = importlib.util.module_from_spec(spec)
loader.exec_module(ad)

# A persistent harness provides a real, stable /proc ancestor across CLI calls.
WORKER = r'''
import ctypes, json, os, subprocess, sys
ctypes.CDLL(None).prctl(15, b'codex', 0, 0, 0)
print('ready', flush=True)
for line in sys.stdin:
    request = json.loads(line)
    env = dict(os.environ)
    env.update(request.get('env', {}))
    result = subprocess.run([sys.executable, sys.argv[1], *request['args']],
                            env=env, capture_output=True, text=True)
    print(json.dumps({'code': result.returncode, 'out': result.stdout,
                      'err': result.stderr}), flush=True)
'''


class CloseOwnershipTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='adt-ownership-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runtime = self.root / 'runtime'
        self.cache = self.root / 'cache'
        self.workers = []
        for thread in ('thread-a', 'thread-b'):
            env = dict(os.environ, AGENT_DESKTOP_RUNTIME=str(self.runtime),
                       AGENT_DESKTOP_CACHE=str(self.cache), CODEX_THREAD_ID=thread)
            p = subprocess.Popen([sys.executable, '-u', '-c', WORKER, str(CLI)],
                                 env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True)
            self.workers.append(p)
            self.addCleanup(self.stop_worker, p)
            self.assertEqual(p.stdout.readline().strip(), 'ready')

    def stop_worker(self, p):
        p.stdin.close()
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait(timeout=5)
        p.stdout.close()
        p.stderr.close()

    def identity(self, index=0):
        return {'pid': self.workers[index].pid,
                'start': ad.proc_start(self.workers[index].pid),
                'codex_thread_id': ('thread-a', 'thread-b')[index]}

    def workspace(self, name, creator, **extra):
        directory = self.runtime / 'ws' / name
        directory.mkdir(parents=True)
        state = dict(name=name, id='ownership-test-' + name, display=100,
                     pids={}, starts={}, creator=creator)
        state.update(extra)
        (directory / 'state.json').write_text(json.dumps(state))
        files = self.cache / 'ws' / name
        files.mkdir(parents=True)
        (files / 'sentinel').write_text(name)
        return directory, files

    def call(self, *args, worker=0, **env):
        p = self.workers[worker]
        p.stdin.write(json.dumps({'args': args, 'env': env}) + '\n')
        p.stdin.flush()
        return json.loads(p.stdout.readline())

    def test_creator_can_close_and_keep_files(self):
        directory, files = self.workspace('mine', self.identity())
        result = self.call('close', 'mine', '--keep-files')
        self.assertEqual(result['code'], 0, result)
        self.assertFalse(directory.exists())
        self.assertTrue((files / 'sentinel').exists())

    def test_foreign_creator_refused_even_with_matching_cleanup_owner(self):
        directory, files = self.workspace('other', self.identity(1),
                                          owner_pid=self.workers[0].pid)
        result = self.call('close', 'other')
        self.assertEqual(result['code'], 3, result)
        self.assertIn('not created by this agent session', result['err'])
        self.assertTrue(directory.exists())
        self.assertTrue((files / 'sentinel').exists())
        self.assertFalse((self.cache / 'events.log').exists())
        self.assertEqual(self.call('close', 'other', worker=1)['code'], 0)

    def test_pid_reuse_thread_change_and_legacy_state_fail_closed(self):
        original = self.identity()
        variants = [dict(original, start=original['start'] + 1),
                    dict(original, codex_thread_id='other-thread'), None]
        for index, creator in enumerate(variants):
            with self.subTest(creator=creator):
                name = f'foreign-{index}'
                directory, files = self.workspace(name, creator)
                if creator is None:
                    state_file = directory / 'state.json'
                    state = json.loads(state_file.read_text())
                    del state['creator']
                    state['owner_pid'] = original['pid']
                    state['owner_start'] = original['start']
                    state_file.write_text(json.dumps(state))
                result = self.call('close', name)
                self.assertEqual(result['code'], 3, result)
                self.assertTrue(directory.exists())
                self.assertTrue(files.exists())
        directory, _ = self.workspace('missing-thread', original)
        self.assertEqual(self.call('close', 'missing-thread', CODEX_THREAD_ID='')['code'], 3)
        self.assertTrue(directory.exists())

    def test_all_closes_only_creator_workspaces(self):
        mine, files = self.workspace('mine', self.identity())
        other, other_files = self.workspace('other', self.identity(1))
        legacy, _ = self.workspace('legacy', None)
        result = self.call('close', '--all')
        self.assertEqual(result['code'], 0, result)
        self.assertEqual(result['out'], 'closed mine\n')
        self.assertFalse(mine.exists())
        self.assertFalse(files.exists())
        self.assertTrue(other.exists())
        self.assertTrue(other_files.exists())
        self.assertTrue(legacy.exists())
        self.assertEqual(self.call('close', '--all')['code'], 0)

    def test_explicit_mixed_names_check_each_workspace(self):
        mine, _ = self.workspace('mine', self.identity())
        other, _ = self.workspace('other', self.identity(1))
        result = self.call('close', 'other', 'mine')
        self.assertEqual(result['code'], 3, result)
        self.assertTrue(other.exists())
        self.assertFalse(mine.exists())

    def test_all_with_names_refused_without_closing(self):
        directory, _ = self.workspace('mine', self.identity())
        self.assertEqual(self.call('close', 'mine', '--all')['code'], 2)
        self.assertTrue(directory.exists())


class CreatorIdentityTests(unittest.TestCase):
    def test_no_harness_binds_to_parent_and_requires_live_identity(self):
        with patch.object(ad, 'find_owner_session', return_value=None), \
                patch.object(ad.os, 'getppid', return_value=123), \
                patch.object(ad, 'proc_start', return_value=456), \
                patch.object(ad, 'comm', return_value='bash'):
            self.assertEqual(ad.creator_identity(), {'pid': 123, 'start': 456})
        with patch.object(ad, 'proc_start', return_value=None):
            with self.assertRaises(ad.Fail) as error:
                ad.creator_identity()
            self.assertEqual(error.exception.code, ad.EXIT_REFUSED)

    def test_new_records_creator_independently_of_cleanup_owner(self):
        creator = {'pid': 123, 'start': 456}
        for owner_arg in ('none', 'auto', '789'):
            with self.subTest(owner_pid=owner_arg), tempfile.TemporaryDirectory() as tmp:
                captured = {}

                class Captured(Exception):
                    pass

                def capture_env(workspace):
                    captured.update(workspace.state)
                    raise Captured()

                args = SimpleNamespace(geometry='1280x800', name='new-test', idle=60,
                                       owner_pid=owner_arg, label='test', share_home=False,
                                       audio=False, glx=False)
                with contextlib.ExitStack() as stack:
                    for name, value in [('RUNTIME', Path(tmp)),
                                        ('WS_ROOT', Path(tmp) / 'ws'),
                                        ('CACHE', Path(tmp) / 'cache')]:
                        stack.enter_context(patch.object(ad, name, value))
                    for name, value in [('creator_identity', creator), ('gc', 0),
                                        ('ram_guard', None), ('pick_display', 100),
                                        ('find_owner_session', 789), ('alive', True),
                                        ('proc_start', 999), ('comm', 'other')]:
                        stack.enter_context(patch.object(ad, name, return_value=value))
                    stack.enter_context(patch.object(ad.shutil, 'which', return_value='/tool'))
                    stack.enter_context(patch.object(ad.subprocess, 'run'))
                    stack.enter_context(patch.object(ad.Workspace, 'env', capture_env))
                    with self.assertRaises(Captured):
                        ad.cmd_new(args)
                self.assertEqual(captured['creator'], creator)
                self.assertEqual(captured['owner_pid'], None if owner_arg == 'none' else 789)

    def test_refusal_never_calls_process_or_file_cleanup(self):
        w = SimpleNamespace(name='foreign', state={'creator': {'pid': 2, 'start': 3}},
                            exists=lambda: True)
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(ad, 'RUNTIME', Path(tmp)), \
                patch.object(ad, 'creator_identity', return_value={'pid': 4, 'start': 5}), \
                patch.object(ad, 'Workspace', return_value=w), \
                patch.object(ad, 'close_ws') as close, \
                contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as result:
                ad.cmd_close(SimpleNamespace(all=False, ws=['foreign'], keep_files=False))
            self.assertEqual(result.exception.code, 3)
            close.assert_not_called()


class PrivateFilesTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='adt-private-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def mode(self, p):
        return os.lstat(p).st_mode & 0o777

    def test_private_dir_creates_or_tightens_and_refuses_what_it_does_not_own(self):
        new = ad.private_dir(self.root / 'a' / 'b')
        self.assertEqual(self.mode(new), 0o700)
        readable = self.root / 'readable'
        readable.mkdir()
        os.chmod(readable, 0o755)
        ad.private_dir(readable)
        self.assertEqual(self.mode(readable), 0o700)
        # Others could have planted links in a writable one: refused as is.
        writable = self.root / 'writable'
        writable.mkdir()
        os.chmod(writable, 0o777)
        with self.assertRaises(ad.Fail):
            ad.private_dir(writable)
        self.assertEqual(self.mode(writable), 0o777)
        os.chmod(writable, 0o775)
        with patch.object(ad, 'own_group', return_value=False), self.assertRaises(ad.Fail):
            ad.private_dir(writable)
        with patch.object(ad, 'own_group', return_value=True):
            ad.private_dir(writable)  # umask 002 with a private group
        self.assertEqual(self.mode(writable), 0o700)
        os.symlink(new, self.root / 'link')
        (self.root / 'file').write_text('')
        for bad in ('link', 'file'):
            with self.assertRaises(ad.Fail):
                ad.private_dir(self.root / bad)
        self.assertEqual(self.mode(new), 0o700)

    def test_group_write_through_an_acl_is_refused(self):
        import struct
        d = self.root / 'acl'
        d.mkdir()
        # user::rwx, user:<another uid>:rwx, group::rwx, mask::rwx, other::---
        entries = [(0x01, 7, 0xFFFFFFFF), (0x02, 7, os.getuid() + 1), (0x04, 7, 0xFFFFFFFF),
                   (0x10, 7, 0xFFFFFFFF), (0x20, 0, 0xFFFFFFFF)]
        acl = struct.pack('<I', 2) + b''.join(struct.pack('<HHI', *e) for e in entries)
        try:
            os.setxattr(d, 'system.posix_acl_access', acl)
        except OSError as e:
            self.skipTest(f'no POSIX ACLs here ({e.strerror})')
        self.assertEqual(self.mode(d), 0o770)
        with patch.object(ad, 'own_group', return_value=True), self.assertRaises(ad.Fail):
            ad.private_dir(d)

    def test_acl_lookup_errors_refuse_unless_xattrs_are_unsupported(self):
        import errno
        d = self.root / 'group'
        d.mkdir()
        os.chmod(d, 0o770)
        with patch.object(ad, 'own_group', return_value=True):
            with patch.object(ad.os, 'listxattr', side_effect=OSError(errno.E2BIG, 'too big')), \
                    self.assertRaises(ad.Fail):
                ad.private_dir(d)
            self.assertEqual(self.mode(d), 0o770)
            with patch.object(ad.os, 'listxattr', side_effect=OSError(errno.EOPNOTSUPP, 'unsupported')):
                ad.private_dir(d)
        self.assertEqual(self.mode(d), 0o700)

    def test_workspace_files_and_the_cache_above_them_are_private(self):
        cache = self.root / 'cache'
        old = cache / 'ws' / 'old'
        old.mkdir(parents=True)
        for p in (cache, cache / 'ws', old):
            os.chmod(p, 0o755)  # created before this check, umask 022
        with patch.object(ad, 'CACHE', cache):
            w = ad.Workspace('old')
            ad.private_files(w)
        for p in (cache, cache / 'ws', old):
            self.assertEqual(self.mode(p), 0o700, p)

    def test_closing_keeps_files_in_a_cache_others_can_write(self):
        cache = self.root / 'cache'
        files = cache / 'ws' / 'gone'
        files.mkdir(parents=True)
        (files / 'log').write_text('x')
        os.chmod(cache / 'ws', 0o777)
        state = {'pids': {}, 'starts': {}, 'id': 'x', 'display': 100}
        w = SimpleNamespace(name='gone', state=state, dir=self.root / 'run' / 'ws' / 'gone', files=files)
        with patch.object(ad, 'CACHE', cache), patch.object(ad, 'WS_ROOT', self.root / 'run' / 'ws'), \
                patch.object(ad, 'marked_pids', return_value=[]), \
                contextlib.redirect_stderr(io.StringIO()) as err:
            ad.close_ws(w, 'test')
        self.assertTrue((files / 'log').exists())
        self.assertIn('kept the files of gone', err.getvalue())
        os.chmod(cache / 'ws', 0o755)
        with patch.object(ad, 'CACHE', cache), patch.object(ad, 'WS_ROOT', self.root / 'run' / 'ws'), \
                patch.object(ad, 'marked_pids', return_value=[]):
            ad.close_ws(w, 'test')
        self.assertFalse(files.exists())
        self.assertEqual(self.mode(cache), 0o700)

    def test_runtime_dir_in_tmp_checks_its_parent_too(self):
        parent = self.root / f'agent-desktop-{os.getuid()}'
        parent.mkdir()
        os.chmod(parent, 0o755)
        with patch.object(ad, 'TMP_RUNTIME', parent), \
                patch.object(ad, 'RUNTIME', parent / 'agent-desktop'):
            ad.runtime_dir()
            self.assertEqual(self.mode(parent), 0o700)
            self.assertEqual(self.mode(parent / 'agent-desktop'), 0o700)
            parent.chmod(0o700)
            (parent / 'agent-desktop').rmdir()
            os.symlink(self.root, parent / 'agent-desktop')
            with self.assertRaises(ad.Fail):
                ad.runtime_dir()

    def test_xauth_cookie_is_not_on_the_command_line(self):
        bin_dir = self.root / 'bin'
        bin_dir.mkdir()
        record = self.root / 'record'
        fake = bin_dir / 'xauth'
        fake.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > {record}.argv\ncat > {record}.stdin\n')
        fake.chmod(0o755)
        path = self.root / 'Xauthority'
        with patch.dict(os.environ, {'PATH': f'{bin_dir}:{os.environ.get("PATH", "")}'}):
            ad.write_xauth(path, 123)
        argv = Path(f'{record}.argv').read_text()
        stdin = Path(f'{record}.stdin').read_text()
        cookie = stdin.split()[-1]
        self.assertRegex(stdin, r'^add :123 MIT-MAGIC-COOKIE-1 [0-9a-f]{32}\n$')
        self.assertNotIn(cookie, argv)
        self.assertEqual(argv.split(), ['-q', '-f', str(path), 'source', '-'])
        self.assertEqual(self.mode(path), 0o600)


if __name__ == '__main__':
    unittest.main()

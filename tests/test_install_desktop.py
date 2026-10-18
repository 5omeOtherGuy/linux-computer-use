"""Installer migration and refusal checks; no real user links are changed."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('install_desktop', Path(__file__).resolve().parents[1] / 'scripts/install-desktop.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
REPO = Path(__file__).resolve().parents[1]
PANEL = REPO / 'desktop/panel/install-panel.sh'


class InstallDesktopTests(unittest.TestCase):
    def test_install_is_idempotent_and_links_single_source(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            installer.install(root / 'bin', root / 'skills')
            installer.install(root / 'bin', root / 'skills')
            self.assertEqual((root / 'bin/agent-desktop').resolve(), Path(__file__).resolve().parents[1] / 'desktop/bin/agent-desktop')
            self.assertTrue((root / 'skills/agent-desktop/references').is_dir())

    def test_migrating_links_requires_flag(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            (root / 'bin').mkdir()
            old = root / 'bin/agent-desktop'
            old.symlink_to(root / 'old-checkout')
            with self.assertRaises(SystemExit):
                installer.install(root / 'bin', root / 'skills')
            self.assertFalse((root / 'skills').exists())
            installer.install(root / 'bin', root / 'skills', replace_links=True)
            self.assertTrue(old.resolve().is_file())

    def test_regular_file_refusal_happens_before_any_changes(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            (root / 'bin').mkdir()
            collision = root / 'bin/agent-desktop-menu'
            collision.write_text('keep')
            with self.assertRaises(SystemExit):
                installer.install(root / 'bin', root / 'skills', replace_links=True)
            self.assertFalse((root / 'bin/agent-desktop').exists())
            self.assertEqual(collision.read_text(), 'keep')


class InstallPanelLinkTests(unittest.TestCase):
    """desktop/panel/install-panel.sh with a stub xfconf-query that reports the
    panel item as already installed, so only the command links change."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.bin = self.root / 'bin'
        stubs = self.root / 'stubs'
        stubs.mkdir()
        (stubs / 'xfconf-query').write_text('#!/bin/sh\necho genmon\n')
        (stubs / 'xfconf-query').chmod(0o755)
        (self.root / 'config/agent-desktop').mkdir(parents=True)
        (self.root / 'config/agent-desktop/panel-plugin-id').write_text('7\n')
        self.env = dict(os.environ, HOME=str(self.root), XDG_CONFIG_HOME=str(self.root / 'config'),
                        PATH=f"{stubs}:{os.environ['PATH']}")

    def run_panel(self, *extra):
        return subprocess.run(['bash', str(PANEL), '--apply', '--no-packages', '--no-restart',
                               '--bin-dir', str(self.bin), *extra],
                              env=self.env, capture_output=True, text=True)

    def target(self, name):
        return (self.bin / name).resolve()

    def test_links_are_created_once(self):
        for _ in range(2):
            r = self.run_panel()
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.target('agent-desktop'), REPO / 'desktop/bin/agent-desktop')
        self.assertEqual(self.target('agent-desktop-genmon'), REPO / 'desktop/panel/agent-desktop-genmon')

    def test_existing_files_are_never_replaced(self):
        self.bin.mkdir()
        (self.bin / 'agent-desktop-menu').write_text('keep')
        for extra in ((), ('--replace-links',)):
            r = self.run_panel(*extra)
            self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
            self.assertIn('refusing to replace', r.stdout)
        self.assertEqual((self.bin / 'agent-desktop-menu').read_text(), 'keep')
        self.assertFalse((self.bin / 'agent-desktop').exists(), 'nothing changed before the refusal')

    def test_links_elsewhere_need_the_flag(self):
        self.bin.mkdir()
        other = self.root / 'other-checkout'
        other.write_text('')
        (self.bin / 'agent-desktop').symlink_to(other)
        r = self.run_panel()
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertEqual(self.target('agent-desktop'), other)
        self.assertFalse((self.bin / 'agent-desktop-menu').exists())
        r = self.run_panel('--replace-links')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.target('agent-desktop'), REPO / 'desktop/bin/agent-desktop')

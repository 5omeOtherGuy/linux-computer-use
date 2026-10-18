"""Installer migration and refusal checks; no real user links are changed."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('install_desktop', Path(__file__).resolve().parents[1] / 'scripts/install-desktop.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


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

import os
import subprocess
import sys
from pathlib import Path

import pytest


@pytest.fixture
def desktop_fixture(tmp_path):
    process = subprocess.Popen([sys.executable, str(Path(__file__).with_name("fixture_app.py"))],
                               env=dict(os.environ, LCU_FIXTURE_DIR=str(tmp_path)),
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                               start_new_session=True)
    try:
        assert process.stdout.readline().strip() == "READY"
        yield tmp_path
    finally:
        process.terminate()
        process.wait(timeout=5)


def pytest_configure(config):
    config.addinivalue_line("markers", "desktop: operates the visible test window")

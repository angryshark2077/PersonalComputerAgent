from __future__ import annotations

import json
import ctypes
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
INSPECTOR = ROOT / "scripts/inspect-s1a-processes.py"


class ProcessInspectorTests(unittest.TestCase):
    def test_libproc_binds_exact_parent_and_direct_child_executable_paths(self) -> None:
        with tempfile.TemporaryDirectory(prefix="pca-libproc.") as temporary_directory:
            ready = Path(temporary_directory) / "ready"
            parent_code = (
                "import pathlib,subprocess,sys,time; "
                "child=subprocess.Popen([sys.argv[2],'-c','import time; time.sleep(60)']); "
                "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); time.sleep(60)"
            )
            libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
            current_path_buffer = ctypes.create_string_buffer(4096)
            self.assertGreater(libproc.proc_pidpath(os.getpid(), current_path_buffer, len(current_path_buffer)), 0)
            fixture_executable = os.fsdecode(current_path_buffer.value)
            parent = subprocess.Popen(
                [fixture_executable, "-c", parent_code, str(ready), fixture_executable], start_new_session=True,
            )
            try:
                for _ in range(100):
                    if ready.exists():
                        break
                    time.sleep(0.01)
                self.assertTrue(ready.exists(), "fixture child did not start")
                path_buffer = ctypes.create_string_buffer(4096)
                self.assertGreater(libproc.proc_pidpath(parent.pid, path_buffer, len(path_buffer)), 0)
                executable = Path(os.fsdecode(path_buffer.value))
                result = subprocess.run(
                    [
                        sys.executable, str(INSPECTOR), "--agent-pid", str(parent.pid), "--uid", str(os.geteuid()),
                        "--agent-path", str(executable), "--bridge-path", str(executable),
                    ],
                    check=False, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue(result.stdout.strip(), "process inspector succeeded without JSON output")
                document = json.loads(result.stdout)
                self.assertEqual(document["agent"]["pid"], parent.pid)
                self.assertEqual(document["bridge"]["ppid"], parent.pid)
            finally:
                try:
                    os.killpg(parent.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                parent.wait()


if __name__ == "__main__":
    unittest.main()

from __future__ import annotations

import os
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "scripts" / "run-with-timeout.py"


class RunWithTimeoutTests(unittest.TestCase):
    def test_timeout_kills_and_reaps_the_entire_process_group(self) -> None:
        with tempfile.TemporaryDirectory(prefix="pca-timeout.") as temporary_directory:
            pid_file = Path(temporary_directory) / "pids"
            fixture = (
                "import os, subprocess, sys, time; "
                "child=subprocess.Popen([sys.executable, '-c', "
                "'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)']); "
                "open(sys.argv[1], 'w').write(f'{os.getpid()} {child.pid}'); "
                "time.sleep(60)"
            )
            started = time.monotonic()
            result = subprocess.run(
                [str(RUNNER), "--timeout", "1.5", "--", "python3", "-c", fixture, str(pid_file)],
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            elapsed = time.monotonic() - started

            self.assertEqual(result.returncode, 124, result.stdout)
            self.assertLess(elapsed, 3.5)
            parent_pid, child_pid = (int(value) for value in pid_file.read_text().split())
            for pid in (parent_pid, child_pid):
                with self.subTest(pid=pid):
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)


if __name__ == "__main__":
    unittest.main()

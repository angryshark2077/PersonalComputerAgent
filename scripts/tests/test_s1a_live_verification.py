from __future__ import annotations

import json
import os
import plistlib
import shutil
import socket
import stat
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
VERIFY = ROOT / "scripts" / "verify-s1a-live.sh"


class S1ALiveVerificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = Path(tempfile.mkdtemp(prefix="pca-live.", dir="/private/tmp"))
        self.addCleanup(lambda: shutil.rmtree(self.temporary_directory, ignore_errors=True))
        self.runtime_root = self.temporary_directory / "Application Support" / "PersonalComputerAgent"
        self.app = self.runtime_root / "App" / "PersonalComputerAgent.app"
        self.data = self.runtime_root / "Data"
        self.run = self.runtime_root / "Run"
        self.agent = self.app / "Contents/Resources/bin/pca-agentd"
        self.bridge = self.app / "Contents/Resources/bin/PCAPlatformBridge"
        self.socket_path = self.run / "bridge.sock"
        self.database = self.data / "agent.sqlite3"
        self.status = self.run / "runtime-status.json"
        self.tools = self.temporary_directory / "tools"
        self.tools.mkdir()
        self.tool_log = self.temporary_directory / "tools.log"
        self._make_layout()
        self._make_tools()

    def run_verify(
        self,
        *arguments: str,
        environment_updates: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "PATH": f"{self.tools}{os.pathsep}{environment['PATH']}",
                "PCA_S1A_LIVE_TEST_MODE": "1",
                "PCA_S1A_LIVE_TEST_ROOT": str(self.runtime_root),
                "PCA_S1A_LIVE_TEST_INSTALL_POLLS": "1",
                "PCA_S1A_LIVE_TEST_HEALTH_POLLS": "1",
                "PCA_S1A_LIVE_TEST_POLL_SECONDS": "0",
                "PCA_S1A_LIVE_TEST_BUNDLE_VERIFIER": str(self.tools / "bundle-verifier"),
                "PCA_S1A_LIVE_TEST_TOOL_LOG": str(self.tool_log),
                "PCA_S1A_LIVE_TEST_AGENT_PATH": str(self.agent),
                "PCA_S1A_LIVE_TEST_BRIDGE_PATH": str(self.bridge),
                "PCA_S1A_LIVE_TEST_SOCKET_PATH": str(self.socket_path),
                "PCA_S1A_LIVE_TEST_AGENT_PID": "4101",
                "PCA_S1A_LIVE_TEST_BRIDGE_PID": "4102",
                "PCA_S1A_LIVE_TEST_AGENT_UID": str(os.geteuid()),
                "PCA_S1A_LIVE_TEST_BRIDGE_UID": str(os.geteuid()),
                "PCA_S1A_LIVE_TEST_DUPLICATE_AGENT": "0",
                "PCA_S1A_LIVE_TEST_JOB_STATE": "running",
                "PCA_S1A_LIVE_TEST_JOB_PROGRAM": str(self.agent),
            }
        )
        if environment_updates:
            environment.update(environment_updates)
        return subprocess.run(
            [str(VERIFY), *arguments],
            cwd=ROOT,
            env=environment,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    def test_installed_mode_accepts_one_exact_healthy_runtime(self) -> None:
        result = self.run_verify("--installed")

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("S1A LIVE VERIFIED", result.stdout)
        log = self.tool_log.read_text(encoding="utf-8") if self.tool_log.exists() else ""
        self.assertNotIn("open ", log)

    def test_usage_requires_exactly_one_supported_mode(self) -> None:
        for arguments in ((), ("--installed", "extra"), ("--dmg", "relative.dmg")):
            with self.subTest(arguments=arguments):
                result = self.run_verify(*arguments)
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("usage:", result.stdout)

    def test_dmg_is_verified_before_plain_open_and_then_checks_installed_runtime(self) -> None:
        dmg = self.temporary_directory / "PersonalComputerAgent-S1A-arm64.dmg"
        dmg.write_bytes(b"synthetic dmg")

        result = self.run_verify(
            "--dmg",
            str(dmg),
            environment_updates={"PCA_TEAM_ID": "ABCDEFGHIJ"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        lines = self.tool_log.read_text(encoding="utf-8").splitlines()
        self.assertEqual(lines[0], f"bundle-verifier --team-id ABCDEFGHIJ {dmg}")
        self.assertEqual(lines[1], f"open {dmg}")
        self.assertFalse(any("spctl" in line or "xattr" in line for line in lines))

    def test_ambiguous_agent_processes_fail_closed(self) -> None:
        result = self.run_verify(
            "--installed",
            environment_updates={"PCA_S1A_LIVE_TEST_DUPLICATE_AGENT": "1"},
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one agent", result.stdout)

    def test_root_bridge_process_is_rejected(self) -> None:
        result = self.run_verify(
            "--installed",
            environment_updates={"PCA_S1A_LIVE_TEST_BRIDGE_UID": "0"},
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("current user", result.stdout)

    def test_stale_heartbeat_is_rejected(self) -> None:
        payload = json.loads(self.status.read_text(encoding="utf-8"))
        payload["heartbeat_at"] = (datetime.now(timezone.utc) - timedelta(seconds=30)).isoformat()
        self.status.write_text(json.dumps(payload), encoding="utf-8")

        result = self.run_verify("--installed")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("healthy runtime status", result.stdout)

    def test_runtime_files_may_appear_during_the_five_second_health_wait(self) -> None:
        pending_status = self.status.with_suffix(".pending")
        pending_socket = self.socket_path.with_suffix(".pending")
        pending_database = self.database.with_suffix(".pending")
        self.status.rename(pending_status)
        self.socket_path.rename(pending_socket)
        self.database.rename(pending_database)

        result = self.run_verify(
            "--installed",
            environment_updates={
                "PCA_S1A_LIVE_TEST_HEALTH_POLLS": "2",
                "PCA_S1A_LIVE_TEST_RESTORE_RUNTIME": "1",
                "PCA_S1A_LIVE_TEST_PENDING_STATUS": str(pending_status),
                "PCA_S1A_LIVE_TEST_PENDING_SOCKET": str(pending_socket),
                "PCA_S1A_LIVE_TEST_PENDING_DATABASE": str(pending_database),
                "PCA_S1A_LIVE_TEST_TARGET_STATUS": str(self.status),
                "PCA_S1A_LIVE_TEST_TARGET_SOCKET": str(self.socket_path),
                "PCA_S1A_LIVE_TEST_TARGET_DATABASE": str(self.database),
            },
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_wrong_schema_or_app_version_is_rejected(self) -> None:
        for field, value in (("schema_version", 2), ("app_version", "9.9.9")):
            with self.subTest(field=field):
                self._write_status()
                payload = json.loads(self.status.read_text(encoding="utf-8"))
                payload[field] = value
                self.status.write_text(json.dumps(payload), encoding="utf-8")

                result = self.run_verify("--installed")

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("healthy runtime status", result.stdout)

    def test_socket_and_database_must_both_be_mode_0600(self) -> None:
        for path in (self.socket_path, self.database):
            with self.subTest(path=path.name):
                path.chmod(0o644)
                result = self.run_verify("--installed")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("0600", result.stdout)
                path.chmod(0o600)

    def test_symlinked_runtime_directory_is_rejected(self) -> None:
        real_data = self.runtime_root / "RealData"
        self.data.rename(real_data)
        self.data.symlink_to(real_data, target_is_directory=True)

        result = self.run_verify("--installed")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("layout", result.stdout.lower())

    def test_launch_job_must_be_running_the_exact_installed_agent(self) -> None:
        result = self.run_verify(
            "--installed",
            environment_updates={"PCA_S1A_LIVE_TEST_JOB_PROGRAM": "/tmp/wrong-agent"},
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("launchd job", result.stdout)

    def _make_layout(self) -> None:
        for directory in (self.runtime_root, self.app.parent, self.data, self.run):
            directory.mkdir(parents=True, exist_ok=True)
            directory.chmod(0o700)
        for executable in (
            self.app / "Contents/MacOS/PersonalComputerAgent",
            self.agent,
            self.bridge,
        ):
            executable.parent.mkdir(parents=True, exist_ok=True)
            executable.write_bytes(b"synthetic executable")
            executable.chmod(0o755)
        launch_agent = self.app / "Contents/Library/LaunchAgents/com.pca.agentd.plist"
        launch_agent.parent.mkdir(parents=True, exist_ok=True)
        with launch_agent.open("wb") as output:
            plistlib.dump(
                {
                    "Label": "com.pca.agentd",
                    "BundleProgram": "Contents/Resources/bin/pca-agentd",
                    "ProgramArguments": ["pca-agentd", "run"],
                    "RunAtLoad": True,
                    "KeepAlive": True,
                },
                output,
            )
        launch_agent.chmod(0o644)
        info = self.app / "Contents/Info.plist"
        with info.open("wb") as output:
            plistlib.dump(
                {
                    "CFBundleIdentifier": "com.pca.PersonalComputerAgent",
                    "CFBundleShortVersionString": "0.1.0",
                },
                output,
            )

        self.database.write_bytes(b"synthetic sqlite fixture")
        self.database.chmod(0o600)
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.socket_path))
        self.addCleanup(listener.close)
        self.socket_path.chmod(0o600)
        self._write_status()

    def _write_status(self) -> None:
        self.status.write_text(
            json.dumps(
                {
                    "agent_status": "unpaired",
                    "bridge_status": "ready",
                    "local_healthy": True,
                    "heartbeat_at": datetime.now(timezone.utc).isoformat(),
                    "process_id": 4101,
                    "app_version": "0.1.0",
                    "schema_version": 1,
                }
            ),
            encoding="utf-8",
        )
        self.status.chmod(0o600)

    def _make_tools(self) -> None:
        self._write_tool(
            "launchctl",
            """#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == "print" && "$2" == "gui/$(id -u)/com.pca.agentd" ]]
cat <<EOF
com.pca.agentd = {
    state = ${PCA_S1A_LIVE_TEST_JOB_STATE:?}
    program = ${PCA_S1A_LIVE_TEST_JOB_PROGRAM:?}
    pid = ${PCA_S1A_LIVE_TEST_AGENT_PID:?}
}
EOF
""",
        )
        self._write_tool(
            "ps",
            """#!/usr/bin/env bash
set -euo pipefail
agent_pid=${PCA_S1A_LIVE_TEST_AGENT_PID:?}
bridge_pid=${PCA_S1A_LIVE_TEST_BRIDGE_PID:?}
agent_uid=${PCA_S1A_LIVE_TEST_AGENT_UID:?}
bridge_uid=${PCA_S1A_LIVE_TEST_BRIDGE_UID:?}
agent=${PCA_S1A_LIVE_TEST_AGENT_PATH:?}
bridge=${PCA_S1A_LIVE_TEST_BRIDGE_PATH:?}
socket=${PCA_S1A_LIVE_TEST_SOCKET_PATH:?}
if [[ "$*" == "-axo pid=,ppid=,uid=,comm=" ]]; then
  printf '%s %s %s %s\n' "$agent_pid" 1 "$agent_uid" "$agent"
  printf '%s %s %s %s\n' "$bridge_pid" "$agent_pid" "$bridge_uid" "$bridge"
  if [[ "${PCA_S1A_LIVE_TEST_DUPLICATE_AGENT:-0}" == "1" ]]; then
    printf '%s %s %s %s\n' 4199 1 "$agent_uid" "$agent"
  fi
elif [[ "$1" == "-p" && "$3" == "-o" && "$4" == "args=" ]]; then
  if [[ "$2" == "$agent_pid" ]]; then printf '%s run\n' "$agent"; else printf '%s --socket %s\n' "$bridge" "$socket"; fi
else
  exit 64
fi
""",
        )
        self._write_tool(
            "sqlite3",
            """#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == "-readonly" ]]
echo ok
echo 0000:completed
echo 0001:completed
""",
        )
        self._write_tool(
            "open",
            """#!/usr/bin/env bash
set -euo pipefail
[[ $# -eq 1 && "$1" == /*.dmg ]]
echo "open $1" >> "${PCA_S1A_LIVE_TEST_TOOL_LOG:?}"
""",
        )
        self._write_tool(
            "sleep",
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${PCA_S1A_LIVE_TEST_RESTORE_RUNTIME:-0}" == "1" ]]; then
  mv "${PCA_S1A_LIVE_TEST_PENDING_STATUS:?}" "${PCA_S1A_LIVE_TEST_TARGET_STATUS:?}"
  mv "${PCA_S1A_LIVE_TEST_PENDING_SOCKET:?}" "${PCA_S1A_LIVE_TEST_TARGET_SOCKET:?}"
  mv "${PCA_S1A_LIVE_TEST_PENDING_DATABASE:?}" "${PCA_S1A_LIVE_TEST_TARGET_DATABASE:?}"
  export PCA_S1A_LIVE_TEST_RESTORE_RUNTIME=0
fi
""",
        )
        self._write_tool(
            "bundle-verifier",
            """#!/usr/bin/env bash
set -euo pipefail
echo "bundle-verifier $*" >> "${PCA_S1A_LIVE_TEST_TOOL_LOG:?}"
[[ $# -eq 3 && "$1" == "--team-id" && "$2" == "ABCDEFGHIJ" && "$3" == /*.dmg ]]
""",
        )

    def _write_tool(self, name: str, contents: str) -> None:
        path = self.tools / name
        path.write_text(contents, encoding="utf-8")
        path.chmod(path.stat().st_mode | stat.S_IXUSR)


if __name__ == "__main__":
    unittest.main()

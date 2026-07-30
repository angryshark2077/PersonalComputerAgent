from __future__ import annotations

import os
import plistlib
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Optional


ROOT = Path(__file__).resolve().parents[2]
VERIFY = ROOT / "scripts" / "verify-s1a-bundle.sh"
BUILD = ROOT / "scripts" / "build-s1a-dmg.sh"


class S1APackagingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = Path(tempfile.mkdtemp(prefix="pca-package-test."))
        self.tools = self.temp / "tools"
        self.tools.mkdir()
        self.app = self.temp / "Install Personal Computer Agent.app"
        self._make_tools()
        self._make_app()

    def tearDown(self) -> None:
        shutil.rmtree(self.temp)

    def run_verify(
        self,
        input_path: Optional[Path] = None,
        *,
        signature_failure: str = "",
        unexpected_dmg_payload: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["PATH"] = f"{self.tools}:{env['PATH']}"
        env["PCA_SYNTHETIC_SIGNATURE_FAILURE"] = signature_failure
        env["PCA_SYNTHETIC_DMG_APP"] = str(self.app)
        env["PCA_SYNTHETIC_HDIUTIL_LOG"] = str(self.temp / "hdiutil.log")
        env["PCA_SYNTHETIC_UNEXPECTED_PAYLOAD"] = "1" if unexpected_dmg_payload else "0"
        return subprocess.run(
            [str(VERIFY), str(input_path or self.app)],
            cwd=ROOT,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

    def test_valid_synthetic_app_passes_behavioral_verification(self) -> None:
        result = self.run_verify()
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_missing_bridge_is_rejected(self) -> None:
        (self.app / "Contents/Resources/bin/PCAPlatformBridge").unlink()
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("PCAPlatformBridge", result.stdout)

    def test_non_arm64_binary_is_rejected(self) -> None:
        (self.app / "Contents/Resources/bin/pca-agentd.arch").write_text("x86_64\n")
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("arm64", result.stdout)

    def test_missing_launch_agent_plist_is_rejected(self) -> None:
        (self.app / "Contents/Library/LaunchAgents/com.pca.agentd.plist").unlink()
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("com.pca.agentd.plist", result.stdout)

    def test_writable_data_directory_inside_bundle_is_rejected(self) -> None:
        data = self.app / "Contents/Resources/Data"
        data.mkdir()
        data.chmod(0o700)
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Data or Run", result.stdout)

    def test_failed_signature_verification_is_rejected(self) -> None:
        result = self.run_verify(signature_failure=self.app.name)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("signature", result.stdout.lower())

    def test_dmg_is_mounted_readonly_and_unexpected_payload_is_rejected(self) -> None:
        dmg = self.temp / "fixture.dmg"
        dmg.write_bytes(b"synthetic image")
        result = self.run_verify(dmg, unexpected_dmg_payload=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected payload", result.stdout)
        self.assertIn("-readonly", (self.temp / "hdiutil.log").read_text())

    def test_build_fails_before_creating_output_when_identity_is_missing(self) -> None:
        output = self.temp / "must-not-exist.dmg"
        env = os.environ.copy()
        env["PATH"] = f"{self.tools}:{env['PATH']}"
        result = subprocess.run(
            [
                str(BUILD),
                "--team-id",
                "ABCDEFGHIJ",
                "--identity",
                "Apple Development: Missing (ABCDEFGHIJ)",
                "--version",
                "0.1.0",
                "--output",
                str(output),
            ],
            cwd=ROOT,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("identity is not available", result.stdout)
        self.assertFalse(output.exists())

    def _make_app(self) -> None:
        executable = self.app / "Contents/MacOS/PersonalComputerAgent"
        agent = self.app / "Contents/Resources/bin/pca-agentd"
        bridge = self.app / "Contents/Resources/bin/PCAPlatformBridge"
        launch_agent = self.app / "Contents/Library/LaunchAgents/com.pca.agentd.plist"
        launch_agent.parent.mkdir(parents=True)
        agent.parent.mkdir(parents=True)
        executable.parent.mkdir(parents=True)
        for binary in (executable, agent, bridge):
            binary.write_bytes(b"synthetic Mach-O fixture")
            binary.chmod(0o755)
            binary.with_suffix(binary.suffix + ".arch").write_text("arm64\n")
        with (self.app / "Contents/Info.plist").open("wb") as output:
            plistlib.dump(
                {
                    "CFBundleIdentifier": "com.pca.PersonalComputerAgent",
                    "CFBundleShortVersionString": "1.0.0",
                    "CFBundleVersion": "1",
                    "CFBundleExecutable": "PersonalComputerAgent",
                    "LSUIElement": True,
                },
                output,
            )
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

    def _make_tools(self) -> None:
        self._write_tool(
            "codesign",
            """#!/usr/bin/env bash
set -euo pipefail
target="${!#}"
if [[ "$(basename "$target")" == "${PCA_SYNTHETIC_SIGNATURE_FAILURE:-}" ]]; then
  echo "synthetic signature failure" >&2
  exit 1
fi
""",
        )
        self._write_tool(
            "lipo",
            """#!/usr/bin/env bash
set -euo pipefail
target="${!#}"
arch_file="${target}.arch"
[[ -f "$arch_file" ]] || exit 1
cat "$arch_file"
            """,
        )
        self._write_tool(
            "security",
            """#!/usr/bin/env bash
set -euo pipefail
echo "  0 valid identities found"
""",
        )
        self._write_tool(
            "hdiutil",
            """#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "${PCA_SYNTHETIC_HDIUTIL_LOG:?}"
if [[ "$1" == "attach" ]]; then
  mountpoint=""
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == "-mountpoint" ]]; then mountpoint=$2; break; fi
    shift
  done
  [[ -n "$mountpoint" ]]
  cp -R "${PCA_SYNTHETIC_DMG_APP:?}" "$mountpoint/"
  if [[ "${PCA_SYNTHETIC_UNEXPECTED_PAYLOAD:-0}" == "1" ]]; then
    touch "$mountpoint/unexpected.txt"
  fi
fi
""",
        )

    def _write_tool(self, name: str, body: str) -> None:
        path = self.tools / name
        path.write_text(body)
        path.chmod(path.stat().st_mode | stat.S_IXUSR)


if __name__ == "__main__":
    unittest.main()

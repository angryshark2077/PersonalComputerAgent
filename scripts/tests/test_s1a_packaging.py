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
        detach_failure: bool = False,
        traversal_failure: bool = False,
        attach_parse_failure: bool = False,
        private_mount_alias: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["PATH"] = f"{self.tools}:{env['PATH']}"
        env["PCA_SYNTHETIC_SIGNATURE_FAILURE"] = signature_failure
        env["PCA_SYNTHETIC_DMG_APP"] = str(self.app)
        env["PCA_SYNTHETIC_HDIUTIL_LOG"] = str(self.temp / "hdiutil.log")
        env["PCA_SYNTHETIC_UNEXPECTED_PAYLOAD"] = "1" if unexpected_dmg_payload else "0"
        env["PCA_SYNTHETIC_DETACH_FAILURE"] = "1" if detach_failure else "0"
        env["PCA_SYNTHETIC_TRAVERSAL_FAILURE"] = "1" if traversal_failure else "0"
        env["PCA_SYNTHETIC_ATTACH_PARSE_FAILURE"] = "1" if attach_parse_failure else "0"
        env["PCA_SYNTHETIC_PRIVATE_MOUNT_ALIAS"] = "1" if private_mount_alias else "0"
        env["PCA_SYNTHETIC_TEAM_ID"] = "ABCDEFGHIJ"
        env["PCA_SYNTHETIC_TOOL_LOG"] = str(self.temp / "tools.log")
        return subprocess.run(
            [str(VERIFY), "--team-id", "ABCDEFGHIJ", str(input_path or self.app)],
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

    def test_success_reports_machine_readable_candidate_identity(self) -> None:
        result = self.run_verify()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "S1A_BUNDLE_METADATA version=1.0.0 team_id=ABCDEFGHIJ "
            "app_cdhash=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa "
            "main_cdhash=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb "
            "agent_cdhash=cccccccccccccccccccccccccccccccccccccccc "
            "bridge_cdhash=dddddddddddddddddddddddddddddddddddddddd "
            "wechat_repair_cdhash=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee "
            "ffmpeg_cdhash=ffffffffffffffffffffffffffffffffffffffff",
            result.stdout,
        )

    def test_missing_bridge_is_rejected(self) -> None:
        (self.app / "Contents/Resources/bin/PCAPlatformBridge").unlink()
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("PCAPlatformBridge", result.stdout)

    def test_missing_wechat_repair_is_rejected(self) -> None:
        (self.app / "Contents/Resources/bin/pca-wechat-repair").unlink()
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pca-wechat-repair", result.stdout)

    def test_missing_bundled_ffmpeg_is_rejected(self) -> None:
        (self.app / "Contents/Resources/bin/ffmpeg").unlink()
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ffmpeg", result.stdout)

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
        self.assertIn("detach /dev/disk99s1", (self.temp / "hdiutil.log").read_text())

    def test_successful_dmg_verification_detaches_exact_attached_device(self) -> None:
        dmg = self.temp / "fixture.dmg"
        dmg.write_bytes(b"synthetic image")
        result = self.run_verify(dmg)
        self.assertEqual(result.returncode, 0, result.stdout)
        log = (self.temp / "hdiutil.log").read_text()
        self.assertIn("attach -readonly -nobrowse", log)
        self.assertIn("detach /dev/disk99s1", log)

    def test_realpath_equivalent_private_mount_alias_is_accepted(self) -> None:
        dmg = self.temp / "fixture.dmg"
        dmg.write_bytes(b"synthetic image")

        result = self.run_verify(dmg, private_mount_alias=True)

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("detach /dev/disk99s1", (self.temp / "hdiutil.log").read_text())

    def test_attach_parse_failure_still_detaches_requested_mountpoint(self) -> None:
        dmg = self.temp / "fixture.dmg"
        dmg.write_bytes(b"synthetic image")
        result = self.run_verify(dmg, attach_parse_failure=True)
        self.assertNotEqual(result.returncode, 0)
        log = (self.temp / "hdiutil.log").read_text()
        self.assertRegex(log, r"detach .*/mount")

    def test_explicit_detach_failure_fails_verification(self) -> None:
        dmg = self.temp / "fixture.dmg"
        dmg.write_bytes(b"synthetic image")
        result = self.run_verify(dmg, detach_failure=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("detach", result.stdout.lower())

    def test_bundle_traversal_error_is_fatal(self) -> None:
        result = self.run_verify(traversal_failure=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("enumerate", result.stdout.lower())

    def test_non_numeric_three_component_version_is_rejected(self) -> None:
        info = self.app / "Contents/Info.plist"
        with info.open("rb") as source:
            value = plistlib.load(source)
        value["CFBundleShortVersionString"] = "1.0-beta"
        with info.open("wb") as output:
            plistlib.dump(value, output)
        result = self.run_verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version", result.stdout.lower())

    def test_mismatched_nested_team_identifier_is_rejected(self) -> None:
        env_name = "PCA_SYNTHETIC_WRONG_TEAM_TARGET"
        previous = os.environ.get(env_name)
        os.environ[env_name] = "PCAPlatformBridge"
        try:
            result = self.run_verify()
        finally:
            if previous is None:
                os.environ.pop(env_name, None)
            else:
                os.environ[env_name] = previous
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("TeamIdentifier", result.stdout)

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

    def test_build_rejects_existing_output_without_overwriting_it(self) -> None:
        output = self.temp / "existing.dmg"
        output.write_bytes(b"keep")
        env = os.environ.copy()
        env["PATH"] = f"{self.tools}:{env['PATH']}"
        result = subprocess.run(
            [str(BUILD), "--team-id", "ABCDEFGHIJ", "--identity", "Apple Development: Missing (ABCDEFGHIJ)", "--version", "0.1.0", "--output", str(output)],
            cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("output already exists", result.stdout)
        self.assertEqual(output.read_bytes(), b"keep")

    def test_build_never_deletes_preexisting_fixed_prebuilt_directory(self) -> None:
        fixed = ROOT / "platform/macos/.build-inputs/s1a"
        fixed.mkdir(parents=True, exist_ok=True)
        marker = fixed / "user-marker"
        marker.write_text("keep")
        self.addCleanup(lambda: shutil.rmtree(ROOT / "platform/macos/.build-inputs", ignore_errors=True))
        output = self.temp / "blocked.dmg"
        env = os.environ.copy()
        env["PATH"] = f"{self.tools}:{env['PATH']}"
        env["PCA_SYNTHETIC_VALID_IDENTITY"] = "1"
        env["PCA_SYNTHETIC_FAIL_CARGO"] = "1"
        result = subprocess.run(
            [str(BUILD), "--team-id", "ABCDEFGHIJ", "--identity", "Apple Development: Test (ABCDEFGHIJ)", "--version", "0.1.0", "--output", str(output)],
            cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(marker.exists(), result.stdout)

    def test_build_accepts_identity_label_id_that_differs_from_certificate_team_id(self) -> None:
        output = self.temp / "personal-team.dmg"
        identity = "Apple Development: Test (ZYXWVUTSRQ)"
        env = os.environ.copy()
        env["PATH"] = f"{self.tools}:{env['PATH']}"
        env["PCA_SYNTHETIC_VALID_IDENTITY"] = "1"
        env["PCA_SYNTHETIC_IDENTITY_LABEL"] = identity

        result = subprocess.run(
            [
                str(BUILD),
                "--team-id",
                "ABCDEFGHIJ",
                "--identity",
                identity,
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

        self.assertEqual(result.returncode, 77, result.stdout)
        self.assertFalse(output.exists())

    def _make_app(self) -> None:
        executable = self.app / "Contents/MacOS/PersonalComputerAgent"
        agent = self.app / "Contents/Resources/bin/pca-agentd"
        bridge = self.app / "Contents/Resources/bin/PCAPlatformBridge"
        wechat_repair = self.app / "Contents/Resources/bin/pca-wechat-repair"
        ffmpeg = self.app / "Contents/Resources/bin/ffmpeg"
        launch_agent = self.app / "Contents/Library/LaunchAgents/com.pca.agentd.plist"
        launch_agent.parent.mkdir(parents=True)
        agent.parent.mkdir(parents=True)
        executable.parent.mkdir(parents=True)
        for binary in (executable, agent, bridge, wechat_repair, ffmpeg):
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
                    "NSLocationUsageDescription": "Read Wi-Fi identity for location matching.",
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
echo "codesign $*" >> "${PCA_SYNTHETIC_TOOL_LOG:?}"
target="${!#}"
if [[ "$(basename "$target")" == "${PCA_SYNTHETIC_SIGNATURE_FAILURE:-}" ]]; then
  echo "synthetic signature failure" >&2
  exit 1
fi
if [[ "$1" == "--verify" ]]; then
  [[ " $* " == *" --strict "* && " $* " == *" --verbose=2 "* ]]
elif [[ "$1" == "-d" ]]; then
  team="${PCA_SYNTHETIC_TEAM_ID:-ABCDEFGHIJ}"
  if [[ "$(basename "$target")" == "${PCA_SYNTHETIC_WRONG_TEAM_TARGET:-}" ]]; then team="ZZZZZZZZZZ"; fi
  echo "TeamIdentifier=$team" >&2
  case "$(basename "$target")" in
    *.app) cdhash=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ;;
    PersonalComputerAgent) cdhash=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ;;
    pca-agentd) cdhash=cccccccccccccccccccccccccccccccccccccccc ;;
    PCAPlatformBridge) cdhash=dddddddddddddddddddddddddddddddddddddddd ;;
    pca-wechat-repair) cdhash=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee ;;
    ffmpeg) cdhash=ffffffffffffffffffffffffffffffffffffffff ;;
    *) exit 65 ;;
  esac
  echo "CDHash=$cdhash" >&2
fi
""",
        )
        self._write_tool(
            "lipo",
            """#!/usr/bin/env bash
set -euo pipefail
echo "lipo $*" >> "${PCA_SYNTHETIC_TOOL_LOG:?}"
[[ "$1" == "-archs" && $# -eq 2 ]]
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
if [[ "${PCA_SYNTHETIC_VALID_IDENTITY:-0}" == "1" ]]; then
  if [[ "$1" == "find-identity" ]]; then
    identity="${PCA_SYNTHETIC_IDENTITY_LABEL:-Apple Development: Test (ABCDEFGHIJ)}"
    printf '  1) ABCDEF "%s"\n' "$identity"
  else
    echo 'synthetic certificate'
  fi
else
  echo "  0 valid identities found"
fi
""",
        )
        self._write_tool(
            "openssl",
            """#!/usr/bin/env bash
set -euo pipefail
echo 'subject=OU=ABCDEFGHIJ,CN=Apple Development Test'
""",
        )
        self._write_tool(
            "cargo",
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${PCA_SYNTHETIC_FAIL_CARGO:-0}" == "1" ]]; then exit 77; fi
exit 77
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
  reported_mountpoint="$mountpoint"
  if [[ "${PCA_SYNTHETIC_PRIVATE_MOUNT_ALIAS:-0}" == "1" && "$mountpoint" == /var/* ]]; then
    reported_mountpoint="/private$mountpoint"
  fi
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk99</string><key>content-hint</key><string>GUID_partition_scheme</string></dict>
<dict><key>dev-entry</key><string>/dev/disk99s1</string><key>mount-point</key><string>$([[ "${PCA_SYNTHETIC_ATTACH_PARSE_FAILURE:-0}" == "1" ]] && echo /wrong/mount || echo "$reported_mountpoint")</string></dict>
</array></dict></plist>
PLIST
elif [[ "$1" == "detach" ]]; then
  if [[ "${PCA_SYNTHETIC_ATTACH_PARSE_FAILURE:-0}" == "1" ]]; then
    [[ "$2" == */mount ]]
  else
    [[ "$2" == "/dev/disk99s1" ]]
  fi
  [[ "${PCA_SYNTHETIC_DETACH_FAILURE:-0}" != "1" ]]
fi
""",
        )
        self._write_tool(
            "find",
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${PCA_SYNTHETIC_TRAVERSAL_FAILURE:-0}" == "1" ]]; then exit 2; fi
exec /usr/bin/find "$@"
""",
        )

    def _write_tool(self, name: str, body: str) -> None:
        path = self.tools / name
        path.write_text(body)
        path.chmod(path.stat().st_mode | stat.S_IXUSR)


if __name__ == "__main__":
    unittest.main()

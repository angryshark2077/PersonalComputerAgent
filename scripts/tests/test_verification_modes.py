from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class VerificationModeTests(unittest.TestCase):
    def run_script(
        self, script_name: str, environment_updates: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        if environment_updates:
            environment.update(environment_updates)
        return subprocess.run(
            [str(REPOSITORY_ROOT / "scripts" / script_name)],
            cwd=REPOSITORY_ROOT,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

    def path_without(self, executable: str) -> str:
        kept: list[str] = []
        for entry in os.environ.get("PATH", "").split(os.pathsep):
            if entry and not (Path(entry) / executable).exists():
                kept.append(entry)
        return os.pathsep.join(kept)

    def test_structural_mode_never_claims_full_pass(self) -> None:
        result = self.run_script("verify-structural.sh")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("STRUCTURAL VERIFICATION PASSED", result.stdout)
        self.assertNotIn("FULL VERIFICATION PASSED", result.stdout)

    def test_full_mode_fails_when_cargo_is_hidden(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            result = self.run_script(
                "verify-full.sh",
                {
                    "PATH": f"{temporary_directory}{os.pathsep}{self.path_without('cargo')}",
                    "PCA_DISABLE_TOOLCHAIN_FALLBACK": "1",
                },
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing required tool: cargo", result.stderr)
        self.assertNotIn("FULL VERIFICATION PASSED", result.stdout)


if __name__ == "__main__":
    unittest.main()

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class EngineeringGateTests(unittest.TestCase):
    def make_repo(self) -> Path:
        temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(temporary_directory.cleanup)
        return Path(temporary_directory.name)

    @staticmethod
    def write(path: Path, contents: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    @staticmethod
    def run_gate(script_name: str, root: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(REPOSITORY_ROOT / "scripts" / script_name), str(root)],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_duplicate_migration_id_is_rejected(self) -> None:
        root = self.make_repo()
        self.write(root / "crates/db-local/migrations/0000_a.sql", "SELECT 1;")
        self.write(root / "crates/db-local/migrations/0000_b.sql", "SELECT 2;")

        result = self.run_gate("verify_migrations.py", root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate migration id: 0000", result.stderr)

    def test_non_contiguous_migration_ids_are_rejected(self) -> None:
        root = self.make_repo()
        self.write(root / "crates/db-local/migrations/0000_baseline.sql", "SELECT 1;")
        self.write(root / "crates/db-local/migrations/0002_gap.sql", "SELECT 2;")

        result = self.run_gate("verify_migrations.py", root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-monotonic migration ids", result.stderr)

    def test_complete_local_migration_chain_replays_and_prints_checksums(self) -> None:
        root = self.make_repo()
        for relative_path in (
            "crates/db-local/migrations/0000_baseline.sql",
            "crates/db-local/migrations/0001_s1a_runtime.sql",
            "packages/db-cloud/migrations/0000_baseline.sql",
        ):
            source = REPOSITORY_ROOT / relative_path
            self.write(root / relative_path, source.read_text(encoding="utf-8"))

        result = self.run_gate("verify_migrations.py", root)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0000_baseline.sql sha256=", result.stdout)
        self.assertIn("0001_s1a_runtime.sql sha256=", result.stdout)

    def test_domain_to_platform_import_is_rejected(self) -> None:
        root = self.make_repo()
        self.write(root / "crates/domain/src/lib.rs", "use pca_platform::Bridge;")

        result = self.run_gate("verify_boundaries.py", root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/domain -> platform", result.stderr)


if __name__ == "__main__":
    unittest.main()

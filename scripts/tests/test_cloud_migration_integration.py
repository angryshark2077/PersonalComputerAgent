from __future__ import annotations

import shutil
import subprocess
import sys
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class CloudMigrationIntegrationTests(unittest.TestCase):
    def test_postgresql_fresh_replay_upgrade_and_owner_constraints(self) -> None:
        missing = [name for name in ("initdb", "pg_ctl", "psql") if shutil.which(name) is None]
        if missing:
            self.skipTest(f"PostgreSQL binaries unavailable: {', '.join(missing)}")

        result = subprocess.run(
            [
                sys.executable,
                str(REPOSITORY_ROOT / "scripts/verify_cloud_migrations.py"),
                str(REPOSITORY_ROOT),
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("fresh, replay, upgrade, and Owner FK checks passed", result.stdout)


if __name__ == "__main__":
    unittest.main()

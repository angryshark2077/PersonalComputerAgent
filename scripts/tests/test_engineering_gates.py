from __future__ import annotations

import json
import shutil
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
        for migration_root in (
            Path("crates/db-local/migrations"),
            Path("packages/db-cloud/migrations"),
        ):
            for source in sorted((REPOSITORY_ROOT / migration_root).glob("[0-9][0-9][0-9][0-9]_*.sql")):
                relative_path = migration_root / source.name
                self.write(root / relative_path, source.read_text(encoding="utf-8"))

        result = self.run_gate("verify_migrations.py", root)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0000_baseline.sql sha256=", result.stdout)
        self.assertIn("0001_s1a_runtime.sql sha256=", result.stdout)
        self.assertIn("0002_s2_collector_state.sql sha256=", result.stdout)
        self.assertIn("0003_s1b_pairing_state.sql sha256=", result.stdout)
        self.assertIn("0004_s1b_cloud_api_origin.sql sha256=", result.stdout)
        self.assertIn("0010_add_file_messages.sql sha256=", result.stdout)
        self.assertIn("0001_s1b_control_plane.sql sha256=", result.stdout)
        self.assertIn("0002_s1b_device_revocation_audit.sql sha256=", result.stdout)
        self.assertIn("0003_s1b_pairing_state_and_better_auth_session.sql sha256=", result.stdout)
        self.assertIn("0004_s1b_hash_better_auth_sessions.sql sha256=", result.stdout)
        self.assertIn("0005_s2_system_events.sql sha256=", result.stdout)
        self.assertIn("0013_drop_legacy_communication_kind_checks.sql sha256=", result.stdout)
        self.assertIn("0014_local_media_management.sql sha256=", result.stdout)
        self.assertIn("0015_network_locations.sql sha256=", result.stdout)

    def test_domain_to_platform_import_is_rejected(self) -> None:
        root = self.make_repo()
        self.write(root / "crates/domain/src/lib.rs", "use pca_platform::Bridge;")

        result = self.run_gate("verify_boundaries.py", root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/domain -> platform", result.stderr)

    def test_system_collector_forbidden_dependencies_are_rejected(self) -> None:
        for dependency_name, dependency_path, expected_boundary in (
            ("pca-cloud-client", "../cloud-client", "cloud-client"),
            ("pca-db-local", "../db-local", "db-local"),
            ("pca-agentd", "../../agent/core", "agent/core"),
        ):
            with self.subTest(dependency=dependency_name):
                root = self.make_repo()
                self.write(
                    root / "crates/system-collector/Cargo.toml",
                    f"""
[package]
name = "pca-system-collector"
version = "0.0.0"
edition = "2021"

[dependencies]
pca-forbidden = {{ package = "{dependency_name}", path = "{dependency_path}" }}
""",
                )

                result = self.run_gate("verify_boundaries.py", root)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "forbidden dependency: "
                    f"crates/system-collector -> {expected_boundary}",
                    result.stderr,
                )

    def test_system_collector_manifest_pins_sysinfo_and_inherits_msrv(self) -> None:
        cargo = shutil.which("cargo") or "/opt/homebrew/opt/rustup/bin/cargo"
        result = subprocess.run(
            [
                cargo,
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--manifest-path",
                str(REPOSITORY_ROOT / "crates/system-collector/Cargo.toml"),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads(result.stdout)
        package = next(
            package
            for package in metadata["packages"]
            if package["name"] == "pca-system-collector"
        )
        dependency = next(
            dependency
            for dependency in package["dependencies"]
            if dependency["name"] == "sysinfo"
        )

        self.assertEqual(package["rust_version"], "1.82")
        self.assertEqual(dependency["req"], "=0.33.1")
        self.assertIs(dependency["uses_default_features"], False)
        self.assertEqual(dependency["features"], ["system", "disk"])

    def test_contract_gate_installs_postgresql_for_cloud_integration_tests(self) -> None:
        workflow = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")

        self.assertIn("brew install postgresql@17", workflow)
        self.assertIn("brew --prefix postgresql@17", workflow)
        self.assertLess(workflow.index("brew install postgresql@17"), workflow.index("pnpm test"))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import re
import sqlite3
import sys
from pathlib import Path


MIGRATION_ROOTS = (
    Path("crates/db-local/migrations"),
    Path("packages/db-cloud/migrations"),
)

EXPECTED_MIGRATIONS = {
    MIGRATION_ROOTS[0]: [
        "0000_baseline.sql",
        "0001_s1a_runtime.sql",
        "0002_s2_collector_state.sql",
        "0003_s1b_pairing_state.sql",
    ],
    MIGRATION_ROOTS[1]: ["0000_baseline.sql", "0001_s1b_control_plane.sql"],
}


def fail(message: str) -> int:
    print(message, file=sys.stderr)
    return 1


def migrations_in(root: Path) -> list[Path]:
    return sorted(root.glob("[0-9][0-9][0-9][0-9]_*.sql")) if root.is_dir() else []


def duplicate_id(files: list[Path]) -> str | None:
    seen: set[str] = set()
    for path in files:
        migration_id = path.name.split("_", 1)[0]
        if migration_id in seen:
            return migration_id
        seen.add(migration_id)
    return None


def migration_ids(files: list[Path]) -> list[int]:
    return [int(path.name.split("_", 1)[0]) for path in files]


def ids_are_contiguous(files: list[Path]) -> bool:
    identifiers = migration_ids(files)
    return identifiers == list(range(len(identifiers)))


def schema_definitions(connection: sqlite3.Connection) -> list[tuple[str, str, str]]:
    return connection.execute(
        "SELECT type, name, sql FROM sqlite_master "
        "WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name"
    ).fetchall()


def replay_local_chain(files: list[Path]) -> str | None:
    connection = sqlite3.connect(":memory:")
    try:
        connection.execute("PRAGMA foreign_keys = ON")
        for path in files:
            connection.executescript(path.read_text(encoding="utf-8"))
        original_schema = schema_definitions(connection)
        for path in files:
            connection.executescript(path.read_text(encoding="utf-8"))
        replayed_schema = schema_definitions(connection)
        if not original_schema or original_schema != replayed_schema:
            return "local migration chain is not replay-safe"

        for path in files:
            migration_id = path.name.split("_", 1)[0]
            checksum = hashlib.sha256(path.read_bytes()).hexdigest()
            connection.execute(
                "INSERT INTO schema_migrations "
                "(id, checksum, app_version, started_at, completed_at, status) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (migration_id, checksum, "0.0.0", 0, 0, "completed"),
            )
            recorded = connection.execute(
                "SELECT checksum FROM schema_migrations WHERE id = ?", (migration_id,)
            ).fetchone()
            if recorded != (checksum,):
                return f"local migration checksum mismatch: {migration_id}"

        if connection.execute("PRAGMA integrity_check").fetchone() != ("ok",):
            return "local migration chain failed integrity_check"
        if connection.execute("PRAGMA foreign_key_check").fetchall():
            return "local migration chain failed foreign_key_check"
        pairing_columns = [
            row[1] for row in connection.execute("PRAGMA table_info(pairing_state)").fetchall()
        ]
        if pairing_columns != [
            "singleton_id",
            "device_id",
            "workspace_id",
            "credential_ref",
            "credential_generation",
            "applied_control_revision",
            "paired_at_ms",
        ]:
            return f"unexpected pairing_state columns: {pairing_columns}"
        if connection.execute("SELECT COUNT(*) FROM pairing_state").fetchone() != (0,):
            return "pairing_state must remain empty after migration"
    except (OSError, sqlite3.Error, UnicodeDecodeError) as error:
        return f"local migration replay failed: {error}"
    finally:
        connection.close()
    return None


def replay_local_upgrade(files: list[Path]) -> str | None:
    connection = sqlite3.connect(":memory:")
    try:
        connection.execute("PRAGMA foreign_keys = ON")
        for path in files[:-1]:
            connection.executescript(path.read_text(encoding="utf-8"))
        connection.execute(
            "INSERT INTO collector_states ("
            "collector_key, status, version, desired_revision, applied_revision, "
            "created_at_ms, updated_at_ms"
            ") VALUES ('upgrade-sentinel', 'disabled', '0.0.0', 0, 0, 1, 1)"
        )
        connection.executescript(files[-1].read_text(encoding="utf-8"))
        if connection.execute(
            "SELECT collector_key FROM collector_states WHERE collector_key = 'upgrade-sentinel'"
        ).fetchone() != ("upgrade-sentinel",):
            return "local upgrade changed existing Collector state"
        if connection.execute("SELECT COUNT(*) FROM pairing_state").fetchone() != (0,):
            return "local upgrade created pairing state before credential validation"
    except (OSError, sqlite3.Error, UnicodeDecodeError) as error:
        return f"local upgrade replay failed: {error}"
    finally:
        connection.close()
    return None


def validate_cloud_chain(files: list[Path]) -> str | None:
    sql = "\n".join(path.read_text(encoding="utf-8") for path in files[1:])
    expected_tables = {
        "auth_users",
        "auth_sessions",
        "auth_accounts",
        "workspaces",
        "workspace_members",
        "devices",
        "device_credential_generations",
        "pairing_sessions",
        "pairing_authorization_codes",
        "collector_configs",
        "collector_config_audit",
        "device_heartbeats",
    }
    actual_tables = set(
        re.findall(r"CREATE TABLE IF NOT EXISTS\s+([a-z_]+)", sql, flags=re.IGNORECASE)
    )
    if actual_tables != expected_tables:
        return f"unexpected S1B Cloud tables: {sorted(actual_tables)}"
    required_indexes = {
        "idx_pairing_sessions_active_expiry",
        "idx_devices_workspace",
        "idx_collector_config_audit_chronology",
        "idx_device_heartbeats_last",
    }
    actual_indexes = set(
        re.findall(r"CREATE INDEX IF NOT EXISTS\s+([a-z_]+)", sql, flags=re.IGNORECASE)
    )
    if not required_indexes.issubset(actual_indexes):
        return f"missing S1B Cloud indexes: {sorted(required_indexes - actual_indexes)}"
    plaintext_names = re.findall(
        r"\b(access_token|refresh_token|authorization_code|session_token)\b",
        sql,
        flags=re.IGNORECASE,
    )
    if plaintext_names:
        return f"Cloud migration contains plaintext credential columns: {plaintext_names}"
    return None


def main() -> int:
    repository_root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    migration_sets = {
        relative_root: migrations_in(repository_root / relative_root)
        for relative_root in MIGRATION_ROOTS
    }

    for relative_root, files in migration_sets.items():
        repeated_id = duplicate_id(files)
        if repeated_id is not None:
            return fail(f"duplicate migration id: {repeated_id}")
        if files and not ids_are_contiguous(files):
            return fail(
                f"non-monotonic migration ids in {relative_root}: {migration_ids(files)}"
            )

    for relative_root, files in migration_sets.items():
        expected = EXPECTED_MIGRATIONS[relative_root]
        if [path.name for path in files] != expected:
            return fail(f"{relative_root} must contain exactly {', '.join(expected)}")
        for path in files:
            if not path.read_bytes().strip():
                return fail(f"empty migration: {path}")

    replay_error = replay_local_chain(migration_sets[MIGRATION_ROOTS[0]])
    if replay_error is not None:
        return fail(replay_error)
    upgrade_error = replay_local_upgrade(migration_sets[MIGRATION_ROOTS[0]])
    if upgrade_error is not None:
        return fail(upgrade_error)
    cloud_error = validate_cloud_chain(migration_sets[MIGRATION_ROOTS[1]])
    if cloud_error is not None:
        return fail(cloud_error)

    for relative_root, files in migration_sets.items():
        for path in files:
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            print(f"{relative_root}/{path.name} sha256={digest}")
    print("Migration chains passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

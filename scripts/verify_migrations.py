#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import sqlite3
import sys
from pathlib import Path


MIGRATION_ROOTS = (
    Path("crates/db-local/migrations"),
    Path("packages/db-cloud/migrations"),
)


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


def main() -> int:
    repository_root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    migration_sets = {
        relative_root: migrations_in(repository_root / relative_root)
        for relative_root in MIGRATION_ROOTS
    }

    for files in migration_sets.values():
        repeated_id = duplicate_id(files)
        if repeated_id is not None:
            return fail(f"duplicate migration id: {repeated_id}")

    for relative_root, files in migration_sets.items():
        if [path.name for path in files] != ["0000_baseline.sql"]:
            return fail(f"{relative_root} must contain exactly 0000_baseline.sql")
        if not files[0].read_bytes().strip():
            return fail(f"empty migration: {files[0]}")

    local_path = migration_sets[MIGRATION_ROOTS[0]][0]
    local_bytes = local_path.read_bytes()
    checksum = hashlib.sha256(local_bytes).hexdigest()
    connection = sqlite3.connect(":memory:")
    try:
        sql = local_bytes.decode("utf-8")
        connection.executescript(sql)
        original_schema = connection.execute(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'"
        ).fetchone()
        connection.executescript(sql)
        replayed_schema = connection.execute(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'"
        ).fetchone()
        if original_schema is None or original_schema != replayed_schema:
            return fail("local baseline is not replay-safe")
        connection.execute(
            "INSERT INTO schema_migrations "
            "(id, checksum, app_version, started_at, completed_at, status) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            ("0000", checksum, "0.0.0", 0, 0, "completed"),
        )
        recorded = connection.execute(
            "SELECT checksum FROM schema_migrations WHERE id = '0000'"
        ).fetchone()
        if recorded != (checksum,):
            return fail("local migration checksum mismatch")
        if connection.execute("PRAGMA integrity_check").fetchone() != ("ok",):
            return fail("local baseline failed integrity_check")
        if connection.execute("PRAGMA foreign_key_check").fetchall():
            return fail("local baseline failed foreign_key_check")
    except (sqlite3.Error, UnicodeDecodeError) as error:
        return fail(f"local migration replay failed: {error}")
    finally:
        connection.close()

    for relative_root, files in migration_sets.items():
        digest = hashlib.sha256(files[0].read_bytes()).hexdigest()
        print(f"{relative_root}/0000_baseline.sql sha256={digest}")
    print("Migration baselines passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

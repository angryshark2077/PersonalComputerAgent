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

EXPECTED_MIGRATIONS = {
    MIGRATION_ROOTS[0]: ["0000_baseline.sql", "0001_s1a_runtime.sql"],
    MIGRATION_ROOTS[1]: ["0000_baseline.sql"],
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
    except (OSError, sqlite3.Error, UnicodeDecodeError) as error:
        return f"local migration replay failed: {error}"
    finally:
        connection.close()
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

    for relative_root, files in migration_sets.items():
        for path in files:
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            print(f"{relative_root}/{path.name} sha256={digest}")
    print("Migration chains passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

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
        "0004_s1b_cloud_api_origin.sql",
        "0005_wechat_messages.sql",
        "0006_harden_attachment_spool.sql",
        "0007_expand_group_limit.sql",
        "0008_attachment_completion_retention.sql",
        "0009_allow_message_kind_sequence_overlap.sql",
        "0010_add_file_messages.sql",
        "0011_repair_apple_message_idempotency.sql",
        "0012_normalize_apple_message_timestamps.sql",
    ],
    MIGRATION_ROOTS[1]: [
        "0000_baseline.sql",
        "0001_s1b_control_plane.sql",
        "0002_s1b_device_revocation_audit.sql",
        "0003_s1b_pairing_state_and_better_auth_session.sql",
        "0004_s1b_hash_better_auth_sessions.sql",
        "0005_s2_system_events.sql",
        "0006_communication_event_inbox.sql",
        "0007_communication_projections.sql",
        "0008_communication_objects.sql",
        "0009_communication_conversation_names.sql",
        "0010_communication_message_senders.sql",
        "0011_communication_avatars.sql",
        "0012_communication_files.sql",
        "0013_drop_legacy_communication_kind_checks.sql",
        "0014_local_media_management.sql",
        "0015_network_locations.sql",
        "0016_device_location.sql",
        "0017_system_lifecycle_events.sql",
        "0018_network_lifecycle_events.sql",
        "0019_network_changed_events.sql",
        "0020_prefer_complete_media_projections.sql",
        "0021_screen_capture.sql",
        "0022_apple_photos_messages_collectors.sql",
        "0023_photo_library_assets.sql",
        "0024_photo_system_event_constraints.sql",
        "0025_device_network_history.sql",
    ],
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
    replayed_connection = sqlite3.connect(":memory:")
    try:
        connection.execute("PRAGMA foreign_keys = ON")
        replayed_connection.execute("PRAGMA foreign_keys = ON")
        for path in files:
            connection.executescript(path.read_text(encoding="utf-8"))
            replayed_connection.executescript(path.read_text(encoding="utf-8"))
        original_schema = schema_definitions(connection)
        replayed_schema = schema_definitions(replayed_connection)
        if not original_schema or original_schema != replayed_schema:
            return "local migration chain is not deterministic"

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
            "cloud_api_origin",
        ]:
            return f"unexpected pairing_state columns: {pairing_columns}"
        if connection.execute("SELECT COUNT(*) FROM pairing_state").fetchone() != (0,):
            return "pairing_state must remain empty after migration"
    except (OSError, sqlite3.Error, UnicodeDecodeError) as error:
        return f"local migration replay failed: {error}"
    finally:
        connection.close()
        replayed_connection.close()
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
        "device_revocation_audit",
        "device_heartbeats",
        "system_events",
        "communication_events",
        "communication_conversations",
        "communication_messages",
        "communication_message_attachments",
        "communication_objects",
        "device_media_cleanup_requests",
        "device_screenshot_requests",
        "device_screenshots",
        "network_location_library",
        "device_network_history",
        "photo_library_assets",
    }
    actual_tables = set(
        re.findall(
            r"CREATE TABLE (?:IF NOT EXISTS\s+)?([a-z_]+)", sql, flags=re.IGNORECASE
        )
    )
    if actual_tables != expected_tables:
        return f"unexpected S1B Cloud tables: {sorted(actual_tables)}"
    required_indexes = {
        "idx_pairing_sessions_active_expiry",
        "idx_devices_workspace",
        "idx_collector_config_audit_chronology",
        "idx_device_heartbeats_last",
        "idx_system_events_device_chronology",
    }
    actual_indexes = set(
        re.findall(r"CREATE INDEX IF NOT EXISTS\s+([a-z_]+)", sql, flags=re.IGNORECASE)
    )
    if not required_indexes.issubset(actual_indexes):
        return f"missing S1B Cloud indexes: {sorted(required_indexes - actual_indexes)}"
    remediation = next(
        (path.read_text(encoding="utf-8") for path in files if path.name.startswith("0004_")),
        "",
    )
    if not re.search(
        r"ALTER\s+TABLE\s+auth_sessions\s+DROP\s+COLUMN\s+IF\s+EXISTS\s+session_token",
        remediation,
        flags=re.IGNORECASE,
    ):
        return "Cloud session-token remediation must drop the immutable 0003 raw column"
    if not re.search(
        r"ALTER\s+COLUMN\s+session_token_hash\s+SET\s+NOT\s+NULL",
        remediation,
        flags=re.IGNORECASE,
    ):
        return "Cloud session-token remediation must require session_token_hash"
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

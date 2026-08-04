#!/usr/bin/env python3
from __future__ import annotations

import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path


EXPECTED_TABLES = [
    "_pca_migrations",
    "auth_accounts",
    "auth_sessions",
    "auth_users",
    "collector_config_audit",
    "collector_configs",
    "communication_conversations",
    "communication_events",
    "communication_message_attachments",
    "communication_messages",
    "communication_objects",
    "device_credential_generations",
    "device_heartbeats",
    "device_media_cleanup_requests",
    "device_revocation_audit",
    "devices",
    "network_location_library",
    "pairing_authorization_codes",
    "pairing_sessions",
    "system_events",
    "workspace_members",
    "workspaces",
]


class VerificationFailure(RuntimeError):
    pass


def require_binary(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise VerificationFailure(f"required PostgreSQL binary is unavailable: {name}")
    return path


def run(command: list[str], *, expected_success: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    if (result.returncode == 0) != expected_success:
        expectation = "succeed" if expected_success else "fail"
        raise VerificationFailure(
            f"command did not {expectation}: {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def available_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


class TemporaryPostgres:
    def __init__(self, root: Path) -> None:
        self.initdb = require_binary("initdb")
        self.pg_ctl = require_binary("pg_ctl")
        self.psql_binary = require_binary("psql")
        self.data = root / "data"
        self.log = root / "postgres.log"
        self.port = available_port()
        self.user = "pca_migration_test"
        self.started = False

    def start(self) -> None:
        run(
            [
                self.initdb,
                "-D",
                str(self.data),
                "-A",
                "trust",
                "-U",
                self.user,
                "--encoding=UTF8",
                "--no-locale",
                "--no-sync",
            ]
        )
        run(
            [
                self.pg_ctl,
                "-D",
                str(self.data),
                "-l",
                str(self.log),
                "-o",
                f"-F -h 127.0.0.1 -p {self.port}",
                "-w",
                "start",
            ]
        )
        self.started = True

    def stop(self) -> None:
        if self.started:
            run([self.pg_ctl, "-D", str(self.data), "-m", "fast", "-w", "stop"])
            self.started = False

    def psql(
        self,
        database: str,
        *arguments: str,
        expected_success: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        return run(
            [
                self.psql_binary,
                "-X",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                "127.0.0.1",
                "-p",
                str(self.port),
                "-U",
                self.user,
                "-d",
                database,
                *arguments,
            ],
            expected_success=expected_success,
        )


def apply(postgres: TemporaryPostgres, database: str, migration: Path) -> None:
    postgres.psql(database, "-f", str(migration))


def table_names(postgres: TemporaryPostgres, database: str) -> list[str]:
    result = postgres.psql(
        database,
        "-Atc",
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    return result.stdout.splitlines()


def verify_owner_constraints(postgres: TemporaryPostgres, database: str) -> None:
    owner_id = "01983333-7333-8333-8333-333333333333"
    outsider_id = "01987777-7777-8777-8777-777777777777"
    workspace_id = "01982222-7222-8222-8222-222222222222"
    device_id = "01981111-7111-8111-8111-111111111111"
    postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES
          ('{owner_id}', 'Owner', 'owner@example.invalid', now(), now()),
          ('{outsider_id}', 'Outsider', 'outsider@example.invalid', now(), now());
        INSERT INTO workspaces (id, name, slug, created_at, updated_at)
          VALUES ('{workspace_id}', 'Owner Workspace', 'owner-workspace', now(), now());
        INSERT INTO workspace_members (workspace_id, user_id, role, created_at)
          VALUES ('{workspace_id}', '{owner_id}', 'owner', now());
        INSERT INTO pairing_sessions (
          session_id_hash, device_public_key_hash, code_challenge, callback_uri,
          expires_at, created_at, authorized_at
        ) VALUES (
          '{'1' * 64}', '{'2' * 64}', 'challenge',
          'http://127.0.0.1:43123/pca/pair/callback', now() + interval '5 minutes', now(), now()
        );
        """,
    )

    unauthorized_code = postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO pairing_authorization_codes (
          authorization_code_hash, session_id_hash, workspace_id, owner_user_id,
          callback_state_hash, expires_at, created_at
        ) VALUES (
          '{'3' * 64}', '{'1' * 64}', '{workspace_id}', '{outsider_id}',
          '{'4' * 64}', now() + interval '5 minutes', now()
        )
        """,
        expected_success=False,
    )
    if "pairing_codes_owner_membership_fk" not in unauthorized_code.stderr:
        raise VerificationFailure("authorization-code Owner membership FK was not enforced")

    postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO pairing_authorization_codes (
          authorization_code_hash, session_id_hash, workspace_id, owner_user_id,
          callback_state_hash, expires_at, created_at
        ) VALUES (
          '{'3' * 64}', '{'1' * 64}', '{workspace_id}', '{owner_id}',
          '{'4' * 64}', now() + interval '5 minutes', now()
        );
        """,
    )
    unauthorized_device = postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO devices (
          id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at
        ) VALUES (
          '01984444-7444-8444-8444-444444444444', '{workspace_id}', '{outsider_id}',
          '{'5' * 64}', 'macos', now()
        )
        """,
        expected_success=False,
    )
    if "devices_owner_membership_fk" not in unauthorized_device.stderr:
        raise VerificationFailure("device Owner membership FK was not enforced")

    postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO devices (
          id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at
        ) VALUES (
          '{device_id}', '{workspace_id}', '{owner_id}', '{'2' * 64}', 'macos', now()
        );
        INSERT INTO collector_configs (
          workspace_id, device_id, configuration_revision, updated_at
        ) VALUES ('{workspace_id}', '{device_id}', 0, now());
        """,
    )
    unauthorized_audit = postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO collector_config_audit (
          id, workspace_id, device_id, actor_user_id, configuration_revision,
          old_config, new_config, created_at
        ) VALUES (
          '01988888-7888-8888-8888-888888888888', '{workspace_id}', '{device_id}',
          '{outsider_id}', 1, '{{}}', '{{}}', now()
        )
        """,
        expected_success=False,
    )
    if "config_audit_actor_membership_fk" not in unauthorized_audit.stderr:
        raise VerificationFailure("config-audit actor membership FK was not enforced")


def verify_session_secret_remediation(postgres: TemporaryPostgres, database: str) -> None:
    columns = postgres.psql(
        database,
        "-Atc",
        "SELECT column_name || ':' || is_nullable "
        "FROM information_schema.columns "
        "WHERE table_schema = 'public' AND table_name = 'auth_sessions' "
        "ORDER BY column_name",
    ).stdout.splitlines()
    if any(column.startswith("session_token:") for column in columns):
        raise VerificationFailure("raw auth session token column survived remediation")
    if "session_token_hash:NO" not in columns:
        raise VerificationFailure("auth session hash must be required after remediation")
    if postgres.psql(database, "-Atc", "SELECT COUNT(*) FROM auth_sessions").stdout.strip() != "0":
        raise VerificationFailure("legacy raw auth sessions were not invalidated")


def verify_system_lifecycle_constraints(postgres: TemporaryPostgres, database: str) -> None:
    workspace_id = "01982222-7222-8222-8222-222222222222"
    device_id = "01981111-7111-8111-8111-111111111111"
    postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO system_events (
          event_id, workspace_id, device_id, event_type, source, schema_version,
          occurred_at, created_at, sensitivity, payload
        ) VALUES
          ('01989999-7999-8999-8999-999999999991', '{workspace_id}', '{device_id}',
           'agent.started', 'runtime.lifecycle', 1, now(), now(), 'normal', '{{}}'),
          ('01989999-7999-8999-8999-999999999992', '{workspace_id}', '{device_id}',
           'system.sleep', 'runtime.lifecycle', 1, now(), now(), 'normal', '{{}}');
        """,
    )
    wrong_source = postgres.psql(
        database,
        "-c",
        f"""
        INSERT INTO system_events (
          event_id, workspace_id, device_id, event_type, source, schema_version,
          occurred_at, created_at, sensitivity, payload
        ) VALUES (
          '01989999-7999-8999-8999-999999999993', '{workspace_id}', '{device_id}',
          'agent.stopped', 'system', 1, now(), now(), 'normal', '{{}}'
        )
        """,
        expected_success=False,
    )
    if "system_events_source_check" not in wrong_source.stderr:
        raise VerificationFailure("lifecycle event source constraint was not enforced")


def verify(repository_root: Path) -> None:
    migration_directory = repository_root / "packages/db-cloud/migrations"
    migrations = sorted(migration_directory.glob("[0-9][0-9][0-9][0-9]_*.sql"))
    if not migrations:
        raise VerificationFailure(f"no Cloud migrations found in: {migration_directory}")

    with tempfile.TemporaryDirectory(prefix="pca-cloud-migrations-") as temporary:
        postgres = TemporaryPostgres(Path(temporary))
        try:
            postgres.start()
            version = postgres.psql("postgres", "-Atc", "SHOW server_version").stdout.strip()
            postgres.psql("postgres", "-c", "CREATE DATABASE pca_fresh")
            postgres.psql("postgres", "-c", "CREATE DATABASE pca_replay")
            postgres.psql("postgres", "-c", "CREATE DATABASE pca_upgrade")

            for migration in migrations:
                apply(postgres, "pca_fresh", migration)
            verify_session_secret_remediation(postgres, "pca_fresh")
            fresh_before = table_names(postgres, "pca_fresh")
            for migration in migrations:
                apply(postgres, "pca_replay", migration)
            if table_names(postgres, "pca_replay") != fresh_before:
                raise VerificationFailure("replayed Cloud migration chain changed the table set")

            apply(postgres, "pca_upgrade", migrations[0])
            postgres.psql(
                "pca_upgrade",
                "-c",
                "INSERT INTO _pca_migrations "
                "(id, checksum, app_version, started_at, completed_at, status) VALUES "
                "('0000', 'sentinel', '0.0.0', now(), now(), 'completed')",
            )
            for migration in migrations[1:4]:
                apply(postgres, "pca_upgrade", migration)
            postgres.psql(
                "pca_upgrade",
                "-c",
                """
                INSERT INTO auth_users (id, name, email, created_at, updated_at)
                  VALUES ('01980000-7000-8000-8000-000000000001', 'Legacy', 'legacy@example.invalid', now(), now());
                INSERT INTO auth_sessions (
                  id, user_id, session_token_hash, session_token, expires_at, created_at, updated_at
                ) VALUES (
                  '01980000-7000-8000-8000-000000000002', '01980000-7000-8000-8000-000000000001',
                  NULL, 'legacy-raw-session-token', now() + interval '1 day', now(), now()
                );
                """,
            )
            for migration in migrations[4:]:
                apply(postgres, "pca_upgrade", migration)
            if table_names(postgres, "pca_upgrade") != EXPECTED_TABLES:
                raise VerificationFailure("upgrade Cloud migration produced an unexpected table set")
            ledger = postgres.psql(
                "pca_upgrade",
                "-Atc",
                "SELECT checksum FROM _pca_migrations WHERE id = '0000'",
            ).stdout.strip()
            if ledger != "sentinel":
                raise VerificationFailure("Cloud upgrade changed the previous migration ledger")
            verify_session_secret_remediation(postgres, "pca_upgrade")
            verify_owner_constraints(postgres, "pca_upgrade")
            verify_system_lifecycle_constraints(postgres, "pca_upgrade")
            print(f"PostgreSQL {version} fresh, replay, upgrade, and Owner FK checks passed")
        finally:
            postgres.stop()


def main() -> int:
    repository_root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    try:
        verify(repository_root)
    except VerificationFailure as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

# S6 · Privacy, Update and Beta

## Must read

Spec §3, §6, §17.6, §19.3, §20.3, §21.4, §25-27.

## Objective

Close deletion, retention, export, signed update, recovery and operational gates.

## Deliverables

- retention jobs.
- Tombstone propagation.
- object storage physical deletion.
- export pipeline.
- diagnostic bundle with redaction.
- Sparkle feed/signing.
- UpdateCoordinator safe point.
- stop Rust/Bridge.
- SQLite backup/migration/recovery.
- protocol compatibility check.
- Sentry/OTel.
- Beta installer, signing and notarization.
- uninstall/revoke behavior.
- security audit evidence.

## Tests

- offline device attempts resurrection.
- object deletion retry.
- export excludes secrets.
- update during running screenshot/command.
- migration failure restores backup.
- incompatible Bridge blocks unsafe start.
- bad signature rejected.
- revoked device loses cloud access.
- permissions and collector status after update.
- 24h soak.

## Exit gate

- signed/notarized install and update from previous Beta.
- deleted records cannot return.
- migration/update failure recover without data loss.
- all `ACCEPTANCE.md` blockers pass.

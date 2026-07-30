# S5 · Browser, File, Location and WeChat

## Must read

Spec §15, §16, §20-22, §25; ADR-0003; WeChat 4.1.12 runbook.

## Objective

Add extended data sources and a replaceable Rust CommunicationProvider without disturbing Agent Core.

## Deliverables

### Browser

- Chromium extension.
- signed/allowlisted Native Messaging Host.
- URL/title/domain events.
- no Cookie/password/form access.

### File

- Swift FSEvents source.
- Rust Scope/filter/projection.
- metadata only.

### Location

- Core Location Bridge.
- configurable frequency and accuracy.
- no precision overclaim.

### WeChat

- provider state machine.
- process/account/data-source discovery.
- Keychain stored-key validation.
- passive scan capability.
- SQLCipher read-only open.
- session/WAL watcher.
- per-talker sort_seq cursor.
- message persist-before-cursor.
- catch-up and gap error.
- 4.1.12 compatibility result.
- provider health/capabilities.

## WeChat normal-path invariants

- logged out/absent: no UI, no prompt, no app launch.
- passive scan only after product authorization.
- never kill/open/re-sign WeChat.
- never auto-run LLDB Active Extraction.
- unsupported does not degrade Agent Core.
- KeyMaterial never enters SQLite/Event/log.
- no message send/modify.

## Tests

- stored key valid/invalid.
- process appears after agent startup.
- source remains unavailable for hours.
- passive scan fails and backs off.
- session update with multiple messages.
- duplicate watcher notification.
- cursor gap.
- DB shard warning.
- provider crash recovery.
- 4.1.12 matrix.

## Exit gate

- WeChat absent/logged out produces no user interruption.
- supported environment becomes active automatically.
- unsupported returns explicit capability/error.
- no normal-path interference with WeChat.

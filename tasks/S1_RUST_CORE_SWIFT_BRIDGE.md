# S1 · Rust Core + Swift Bridge (split)

> Superseded as an executable task card by
> `tasks/S1A_LOCAL_RUNTIME_INSTALLER.md` and
> `tasks/S1B_CLOUD_CONTROL_PLANE.md`. This file preserves the original S1
> scope as a cross-reference only; it has no independent deliverables or exit
> gate.

## Approved order

1. S1A · Local Runtime and Self-Install DMG
2. S1B · Cloud Control Plane
3. S2 · Core Collectors
4. S3 · Complete Sync

S2 remains before S3. S1A is the only active self-use installation channel:
`$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app`.
It uses a user LaunchAgent and never root or a LaunchDaemon. The future public
channel may use `/Applications` only through a separate decision.

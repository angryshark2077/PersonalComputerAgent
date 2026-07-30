# ADR-0005: User-Level Self-Install Channel

Status: Accepted
Date: 2026-07-31

## Context

The product specification records `/Applications/PersonalComputerAgent.app` as
the bundle location for its signed and notarized public distribution. S1A is a
self-use, Apple Development-signed DMG and needs a channel decision before any
installer, runtime-path, or LaunchAgent implementation is written.

Treating `/Applications` and Application Support as simultaneous S1A targets
would make upgrade, rollback, uninstall, and launchd registration ambiguous.
S1A must also run without a privileged helper or root service.

## Decision

The S1A self-use channel installs to:

```text
~/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app
```

Its fixed sibling directories are:

```text
~/Library/Application Support/PersonalComputerAgent/Data/
~/Library/Application Support/PersonalComputerAgent/Run/
```

S1A uses a user-level `SMAppService` LaunchAgent and never uses root,
LaunchDaemon, or a privileged helper. The S1A bundle is signed with the
developer's Apple Development / Personal Team identity; it is not notarized
and does not use Developer ID, Sparkle, or a quarantine bypass.

The future public channel may return to `/Applications`, but no implementation
may support both locations implicitly. A later ADR and explicit installer
channel selection are required before that channel is implemented.

## Consequences

- Upgrade replaces only `App/`; `Data/` and `Run/` are not bundle locations.
- The installer must request manual Gatekeeper and background-item approval
  when macOS requires it; it must not simulate or bypass either approval.
- S1A target Macs do not need Xcode, Rust, Node, or source code. Full Xcode is
  a development-machine prerequisite only.
- Local uninstall is initiated by the installed executable and preserves data
  by default. S1A also supports complete uninstall through `--delete-data`:
  it displays the exact `Data/` and PCA-owned Keychain credential scope,
  requires an explicit confirmation token, and then deletes only those
  PCA-owned targets.
- S1B adds the cloud control plane after S1A. S2 and S3 retain their approved
  order after S1B.

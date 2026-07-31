# Installation Channels

## S1A self-use channel

S1A is an Apple Silicon, macOS 13+ self-use channel. Its sole installation
root is:

```text
$HOME/Library/Application Support/PersonalComputerAgent
```

The installer and runtime use these exact paths:

```text
$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app
$HOME/Library/Application Support/PersonalComputerAgent/Data/
$HOME/Library/Application Support/PersonalComputerAgent/Run/
```

`App/` contains replaceable program files. `Data/` contains persistent local
facts and logs. `Run/` contains ephemeral locks, sockets, and status. An
upgrade may replace only `App/`; it must not use the install root as a data
directory or erase `Data/`.

The DMG and nested executables are signed with the developer's Apple
Development / Personal Team identity. S1A is intentionally unnotarized and
does not use Developer ID, Sparkle, automatic quarantine removal, Gatekeeper
changes, a privileged helper, root, or a LaunchDaemon.

The installer registers a user-level `SMAppService` LaunchAgent. If macOS
requires Gatekeeper approval or Login Items / Background Items approval, the
user approves it manually in the system UI. The installer opens the relevant
settings and waits for the decision; it never edits approval databases,
invokes `launchctl` to bypass `SMAppService`, or simulates approval.

The live verifier does not change installation, service, permission,
Gatekeeper, TCC, approval-database, or process state. Its `--dmg` mode creates
only a current-user private temporary DMG snapshot, verifies and opens that
same identity-checked file, and removes it with bounded cleanup.

Default local uninstall is the installed executable command:

```bash
"$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app/Contents/MacOS/PersonalComputerAgent" --uninstall
```

It unregisters the user LaunchAgent and removes `App/` and `Run/`, while
preserving `Data/` and PCA-owned Keychain credentials by default. Complete
uninstall is a separate, explicitly confirmed `--delete-data` operation; it
must never delete a parent Application Support directory or unrelated
credentials.

## Future public channel

The product specification's `/Applications/PersonalComputerAgent.app`
location applies to a future public channel, not S1A. That channel requires a
separate ADR and explicit installer selection before it is implemented. No
S1A code may infer or support both locations.

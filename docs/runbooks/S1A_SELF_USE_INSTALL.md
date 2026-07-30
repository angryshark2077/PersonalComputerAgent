# S1A Self-Use Installation and Acceptance

This runbook separates automated read-only checks from the decisions and disruptive checks that only the Mac owner performs. S1A is a self-use, Apple Silicon, macOS 13+ channel. It is not a notarized public installer.

## Safety boundary

Never globally disable Gatekeeper, remove quarantine attributes, edit the Background Items database, approve Login Items by automation, bypass TCC, or run the Agent as root. Do not use `spctl --master-disable`, `xattr -dr com.apple.quarantine`, a root LaunchDaemon, or a privileged helper.

`scripts/verify-s1a-live.sh` is read-only. In `--dmg` mode its only UI action is plain `open <dmg>`; the user still makes every Gatekeeper, install, and Login Items decision. It never registers or unregisters a service, kills a process, deletes data, or changes a permission.

## 1. Signing preflight and local DMG

A current Apple Development identity and its private key must exist in the build Mac's Keychain. The repository and PCA development pack never request, copy, or store the private key, Apple account password, signing password, or Keychain material.

```bash
security find-identity -v -p codesigning
```

If this reports `0 valid identities found`, stop. In Xcode, sign in to the intended Apple ID under Settings > Accounts, select the Personal Team, and create an Apple Development certificate. Then set the matching non-secret Team ID and identity label in the current shell and build:

```bash
export PCA_TEAM_ID='YOUR10CHARTEAMID'
export PCA_APPLE_DEVELOPMENT_IDENTITY='Apple Development: Your Name (YOUR10CHARTEAMID)'
./scripts/build-s1a-dmg.sh \
  --team-id "$PCA_TEAM_ID" \
  --identity "$PCA_APPLE_DEVELOPMENT_IDENTITY" \
  --version 0.1.0 \
  --output dist/PersonalComputerAgent-S1A-arm64.dmg
shasum -a 256 dist/PersonalComputerAgent-S1A-arm64.dmg
```

Record the SHA-256 only after that real build succeeds. Never invent a digest for a missing artifact.

## 2. First graphical install

Run the verifier from the product repository:

```bash
PCA_TEAM_ID="$PCA_TEAM_ID" ./scripts/verify-s1a-live.sh \
  --dmg "$PWD/dist/PersonalComputerAgent-S1A-arm64.dmg"
```

The script first mounts and verifies the bundle read-only, then opens the DMG and waits boundedly for the exact installed path. In Finder:

1. Open **Install Personal Computer Agent.app**.
2. If Gatekeeper blocks the unnotarized Development-signed app, use the normal owner-controlled Open Anyway flow in Privacy & Security, or Control-click the app and choose Open when macOS offers it. Do not disable Gatekeeper globally.
3. Select **Install and Start**.
4. If macOS reports that the background item requires approval, open System Settings > General > Login Items and allow Personal Computer Agent.
5. Return to the installer and wait for success.

The expected code/data/runtime locations are exactly:

```text
~/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app
~/Library/Application Support/PersonalComputerAgent/Data/
~/Library/Application Support/PersonalComputerAgent/Run/
```

The live verifier must print `S1A LIVE VERIFIED` within its bounded wait. Save the terminal output with the date, macOS build, app version, DMG SHA-256, Agent PID, Bridge PID, and UID as the first-install evidence.

## 3. Upgrade and rollback fixture

Before any live upgrade, run the isolated rollback fixture; it uses a temporary test root and does not replace the installed app or its data:

```bash
xcodebuild test \
  -project platform/macos/PersonalComputerAgent.xcodeproj \
  -scheme PersonalComputerAgent \
  -destination 'platform=macOS,arch=arm64' \
  CODE_SIGNING_ALLOWED=NO \
  -only-testing:PersonalComputerAgentTests/InstallCoordinatorTests/testFailedUpgradeHealthRestoresOldBundleWithoutDeletingData
```

For a real upgrade, build a strictly newer signed version with the same Team ID, retain the prior DMG and its SHA-256 as the rollback reference, then repeat the graphical install. Record the installed version before and after with `./scripts/verify-s1a-live.sh --installed`. Do not deliberately corrupt the live bundle or database to force rollback; the isolated fixture is the destructive-failure proof.

## 4. Logout/login recovery

Before logout, record the successful verifier output. Log out normally and log back into the same macOS user. Do not manually start the Agent. Run:

```bash
./scripts/verify-s1a-live.sh --installed
```

A pass proves the approved user-level job started the exact installed Agent and Bridge as the current non-root UID, with a fresh version-matched status, socket mode `0600`, database mode `0600`, and exact completed S1A migration ledger. Record both pre-logout and post-login outputs.

## 5. Exact Bridge termination and recovery

This is a separate, explicit manual acceptance action; the live verifier never kills a process. First capture a passing result and its exact Agent and Bridge PIDs:

```bash
./scripts/verify-s1a-live.sh --installed
```

Confirm the Bridge PID belongs to the exact installed executable before terminating only that PID:

```bash
BRIDGE_PID='PID_FROM_VERIFIER'
ps -p "$BRIDGE_PID" -o pid=,ppid=,uid=,comm=,args=
kill -TERM "$BRIDGE_PID"
```

Observe `Run/runtime-status.json` change away from `bridge_status: ready` and return to `ready`, then rerun:

```bash
./scripts/verify-s1a-live.sh --installed
sqlite3 -readonly "$HOME/Library/Application Support/PersonalComputerAgent/Data/agent.sqlite3" 'PRAGMA integrity_check;'
```

Pass requires a new Bridge PID, the same Rust Agent PID, `PRAGMA integrity_check` equal to `ok`, and the verifier returning success. Do not use `pkill`, `killall`, a name-only PID match, or root.

## 6. Uninstall evidence

Default uninstall removes the app and ephemeral `Run/` state while preserving `Data/` and PCA Keychain credentials:

```bash
"$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app/Contents/MacOS/PersonalComputerAgent" --uninstall
test -d "$HOME/Library/Application Support/PersonalComputerAgent/Data"
```

Reinstall before testing complete uninstall. Complete uninstall prints the exact targets and credential scope before it changes the service. It proceeds only after the literal confirmation token is entered:

```bash
"$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app/Contents/MacOS/PersonalComputerAgent" \
  --uninstall --delete-data
# Type exactly when prompted:
# DELETE PCA DATA
```

Verify that only the PCA-owned `App/`, `Run/`, `Data/`, and PCA Keychain items named by the command were removed. Do not delete the parent `~/Library/Application Support` directory or unrelated Keychain items.

## Evidence status

Unsigned CI, unit tests, packaging fixtures, and this verifier are necessary preparation. They do not substitute for a real Apple Development-signed DMG, the owner's Gatekeeper and Login Items decisions, graphical installation, logout/login, or exact Bridge recovery. Those items remain explicitly blocked until a valid signing identity/private key is available and the owner performs the steps above.

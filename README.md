# Personal Computer Agent

This repository was initialized from the Code Agent Development Pack.

Start with:

- `00_START_HERE.md`
- `tasks/S0_ENGINEERING_BASELINE.md`

The scaffold is intentionally minimal and does not represent completed product functionality.

## Verification

Run the structural checks when compiler toolchains are unavailable:

```bash
./scripts/verify-structural.sh
```

Run the complete S0 engineering gate before claiming the baseline is ready:

```bash
./scripts/verify-full.sh
```

The full gate requires Rust with `rustfmt` and `clippy`, Swift, pnpm 9.15, and Python 3.9 or newer. On Homebrew systems where `rustup` is keg-only, the script uses `/opt/homebrew/opt/rustup/bin` for that process without modifying shell configuration.

## S1A self-use macOS installer

S1A is an Apple Silicon, macOS 13+ source baseline for a per-user runtime. It installs the app at:

```text
~/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app
```

Persistent data stays in sibling `Data/`; ephemeral state stays in sibling `Run/`. The background item is a user-level `SMAppService` LaunchAgent. S1A never installs a root LaunchDaemon, never disables Gatekeeper, and does not bypass Login Items or TCC approval.

Creating the local DMG requires a currently valid Apple Development identity and matching private key in the build Mac's Keychain. The identity, Team ID, private key, Apple account credentials, and DMG are not part of this repository or the PCA source pack. Build only after the preflight reports a valid identity:

```bash
security find-identity -v -p codesigning
./scripts/build-s1a-dmg.sh \
  --team-id "$PCA_TEAM_ID" \
  --identity "$PCA_APPLE_DEVELOPMENT_IDENTITY" \
  --version 0.2.1 \
  --output dist/PersonalComputerAgent-S1A-arm64.dmg
```

The live verifier is read-only except that `--dmg` opens the selected image for the user. It does not approve, register, stop, delete, or change any system permission:

```bash
PCA_TEAM_ID="$PCA_TEAM_ID" ./scripts/verify-s1a-live.sh \
  --dmg "$PWD/dist/PersonalComputerAgent-S1A-arm64.dmg"
./scripts/verify-s1a-live.sh --installed
```

The exact first-install, approval, recovery, logout/login, and uninstall evidence procedure is in [`docs/runbooks/S1A_SELF_USE_INSTALL.md`](docs/runbooks/S1A_SELF_USE_INSTALL.md). A green unsigned CI run proves source/build/test reproducibility; it does not prove a signed DMG, Gatekeeper approval, a real graphical install, or login recovery.

## S1B Railway deployment

The operator-only Railway setup, public-health verification, and the boundary
between deployment and live pairing acceptance are in
[`docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md`](docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md).

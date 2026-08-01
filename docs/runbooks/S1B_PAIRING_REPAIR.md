# S1B Pairing, Revocation, and Repair Runbook

## Operational status and prerequisites

S1B has verified contracts, local fail-closed state, Cloud API tests and
PostgreSQL migration replay. It does **not** yet have a deployed Cloud origin
or an installed Swift Setup-to-Agent 0600 IPC transport/Keychain ACL bootstrap.
Therefore no one should claim that this runbook has executed a live pairing.
The installed Setup UI currently fails closed through
`UnavailablePairingAgentBridge`; it must not open a browser or create a device
credential until that transport is delivered and a production HTTPS origin is
configured.

Before operating pairing, provide all of the following:

1. A deployed HTTPS Cloud API with PostgreSQL migrations applied.
2. Better Auth production secret/configuration in the service's secret store,
   never in this repository or a Dashboard build.
3. A signed local IPC transport restricted to the installed Setup/Repair app
   and `agentd`, plus a Keychain ACL created for that installed identity.
4. An authenticated Owner Dashboard session for the intended Workspace.

## Normal pairing flow

1. Start pairing only from the Setup/Repair app. `agentd` must never launch a
   browser or bind a local HTTP listener.
2. Setup starts one listener on `127.0.0.1` at an ephemeral port and supplies
   the exact `/pca/pair/callback` URI to Agent Core.
3. Agent Core creates PKCE/state/device material and returns only a
   browser-safe HTTPS authorization URL, session ID, and callback state to
   Setup over local IPC.
4. The signed-in Owner authorizes the selected/default Workspace in the
   Dashboard. Cloud redirects a one-use code plus state to the local callback;
   the URL must contain no credential.
5. Setup validates path and state, closes the listener, and forwards only the
   session ID and code to Agent Core. Agent Core exchanges the code, stores the
   credential bundle in Keychain, and writes only the non-secret
   `pairing_state` pointer to SQLite.
6. Agent Core immediately sends authenticated control/heartbeat and then polls
   every 30 seconds. A successful response can apply only a newer complete
   configuration revision. The Owner Dashboard shows device presence and the
   configuration audit actor/device/revision.

Stop and investigate if callback state/path validation fails, the five-minute
window expires, the authorization code is replayed, the configuration is not
for the device Workspace, or any endpoint is not HTTPS. Do not retry by
copying a callback URL or credential into a terminal, log, configuration file,
or SQLite database; begin a fresh pairing session instead.

## Owner configuration

The Owner may configure only the exact `network` and
`communication.wechat` scopes for a device in the Owner Workspace. The WeChat
scope remains fixed to outgoing text/full-sync semantics. Every accepted change
creates an immutable audit record with actor, device, old/new configuration,
revision and time. A Cloud setting is product authorization only; it never
bypasses macOS TCC or enables a source that its later local slice has not
implemented.

## Revoke a device

1. In the authenticated Owner Dashboard, open the intended device and use its
   revoke action. Confirm the device and Workspace before submission.
2. Cloud marks the device and all credential generations revoked and appends a
   `device_revocation_audit` row. This is an irreversible identity operation;
   pairing again creates a fresh credential generation/device record according
   to the Cloud control API.
3. On the next authenticated control request, Agent Core removes its Keychain
   credential, clears local `pairing_state`, and atomically persists disabled
   `network` and `communication.wechat` collector states.
4. Verify from the Dashboard that later control requests are rejected and the
   device is no longer treated as paired. If the Mac is offline, it cannot
   perform the local cleanup until it next runs and contacts Cloud; it must not
   be treated as remotely active in the meantime.

If Keychain deletion reports a local failure, Agent Core still clears the local
pairing state and disables sensitive collectors, then reports the Keychain
failure without logging a credential. Treat the remaining Keychain item as a
local repair condition; do not expose or export it.

## Repair and incident response

- **Expired/malformed/replayed callback:** listener closes; start a fresh
  pairing attempt from Setup/Repair. Never reuse a code.
- **Cloud unavailable:** keep the local S1A runtime healthy; the bounded retry
  backs off up to five minutes. Do not delete valid local credentials merely
  because Cloud is unavailable.
- **Missing/corrupt Keychain credential:** Agent Core clears stale pairing
  state and disables sensitive collectors. Repair requires a fresh pairing
  after prerequisites are available.
- **Confirmed credential revocation:** do not restore SQLite state from backup;
  pair again through the Owner-authorized flow.
- **Suspected secret exposure:** revoke the affected device from the Owner
  Dashboard, rotate the service secret through the deployment secret store if
  applicable, preserve only redacted diagnostics, and create a fresh device
  pairing after containment.

## Verification before declaring an installation paired

Run the repository gate and confirm its PostgreSQL migration evidence and local
shared-state process acceptance:

```bash
CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 ./scripts/verify-full.sh
```

The S1B process acceptance inside this gate starts one synthetic Cloud service
on loopback HTTP. That service alone owns the pairing session and generated
authorization code, exchange-issued credentials, device configuration revision,
audit row and revocation. One live Rust acceptance Agent receives the local API
origin, generates and retains its own PKCE verifier, sends only its derived
challenge and distinct callback state to the shared Cloud session endpoint, then
receives only the callback code from the Node driver before exchanging through
the real endpoint. It sends control over real HTTP, writes the returned synthetic
credential to a file-backed Keychain double, and uses a temporary SQLite
database. Dashboard clients authorize, read, configure and revoke through the
same service. A test-only non-credential JSON response carries the generated
message canary exactly once. Before its final checkpoint, the paired helper scans
its live SQLite main/WAL/SHM files for the callback code, verifier and issued
credentials; the parent retains the final post-revoke scans across process
streams, JSON status, SQLite artifacts and fixture sources.

This proves the local callback-to-revocation state transition, Agent-owned PKCE
continuity and pre-checkpoint temporary-SQLite canary boundary. It does not use
production HTTPS, Better Auth, PostgreSQL, a signed Setup-to-Agent transport, the
macOS Keychain ACL, or Railway networking, and is not evidence that a deployed
installation paired successfully.

For a future live deployment, separately record the deployed API origin,
database migration version, Better Auth login result, one successful callback,
first heartbeat/control revision, Dashboard audit row, and revoke cleanup.
Those deployment observations are not implied by the local synthetic acceptance
or temporary PostgreSQL tests in this repository.

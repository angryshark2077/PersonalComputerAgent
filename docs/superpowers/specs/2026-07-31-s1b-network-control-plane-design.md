# S1B Automatic Pairing and Network Collector Design

**Date:** 2026-07-31
**Status:** Approved for implementation planning
**Supersedes for this private self-use channel:** the manual-code path in product-spec §6.3. ADR-0006 records the deliberate exception.

## 1. Objective and delivery order

Deliver the approved path in three bounded increments:

1. **S1B — Cloud control plane:** automatic browser-based pairing, Keychain-backed device credentials, authenticated presence, and per-device Collector configuration in the Cloud Dashboard. It does not upload business Events.
2. **S2B — Local Network Collector:** the macOS network source, typed `network.changed` Event, durable deduplication checkpoint, and Local Event/Outbox transaction. It has no Cloud geo lookup and no event transport.
3. **S3 — Network sync and location projection:** Event upload, server-observed public egress IP, cloud-only coarse geo enrichment, and Dashboard projections.

This keeps the already-completed local System Collector slice intact. Production System and Network Collectors remain disabled until a real S1B-paired identity and Cloud configuration exist. Tests may continue using the existing debug-only injected identity.

## 2. Approved scope and exclusions

### Included

- Automatic pairing initiated only by the Swift Setup/Repair App.
- Device credentials in macOS Keychain; Device identity, revocation, heartbeat, and desired Collector configuration in Cloud.
- Network interface type, Wi-Fi SSID/BSSID when macOS makes them available, and active-interface local IPv4/IPv6.
- Cloud inference of country/region/city-level location from network data.
- Raw network identifiers retained for 30 days; coarse location projections retained long-term.

### Explicitly excluded

- Battery percentage, charging state, AC state, or any Battery/Power Collector. Existing sleep/wake lifecycle monitoring remains separate.
- Traffic accounting, default-gateway probing, connectivity/reachability checks, latency, captive-portal detection, GPS, exact coordinates, and Agent-side calls to public-IP or geo services.
- Manual pairing-code entry, a normal-path local product dashboard, pairing initiated by `agentd`, and any browser launch by `agentd`.
- S1B business Event upload; S2B Cloud Sync; S3 remote commands or unrelated Collectors.

## 3. Architecture and ownership

```text
Setup/Repair App ── one-time loopback callback ── Browser / Cloud Dashboard
       │                                                    │
       └── Keychain credentials ◀── token exchange ─────────┘
                          │
                  Rust Agent Core ── heartbeat/config ── Cloud API
                          │
             Collector Registry / DbActor / Event + Outbox
                          ▲
        Swift PlatformBridge: NWPathMonitor, CoreWLAN, addresses
```

- **Swift Setup/Repair App** owns the short-lived pairing listener, system-browser launch, Keychain write, and Setup-only recovery UI. It does not touch SQLite or Cloud business data.
- **Rust Agent Core** owns paired/unpaired state, credential references, Cloud-control client, Collector Registry, desired/applied revisions, Event creation, backpressure, and DbActor access.
- **Swift PlatformBridge** owns Apple network APIs and emits/request-serves platform observations through the versioned Bridge contract. It does not make retention, configuration, Cloud, or Event decisions.
- **Network Collector** produces typed observations only. It never calls the Cloud API or writes SQLite directly.
- **Cloud API/worker** own authentication, Workspace scope, revocation, configuration audit, public-IP observation, retention, and the optional geo-provider adapter. The Dashboard never connects to a local Agent.

## 4. S1B automatic pairing and authorization

### 4.1 User flow

1. Setup/Repair detects a missing, invalid, or revoked Keychain device credential while the device is unpaired.
2. It creates a fresh device key pair plus a one-time pairing session, a high-entropy `state`, and a PKCE verifier/challenge. It binds a listener only to `127.0.0.1` on a random ephemeral port with a five-minute maximum lifetime.
3. Setup/Repair opens the system browser to the authenticated Dashboard pairing URL. `agentd` never opens a browser.
4. A signed-in owner is bound automatically to the default Workspace. An owner with multiple Workspaces chooses one once.
5. Cloud redirects a one-time authorization code and `state` to the local listener. The URL contains no access token, refresh secret, or device credential.
6. Setup/Repair verifies the exact callback path and `state`, stops the listener, and exchanges the code, PKCE verifier, and device public key with Cloud over TLS.
7. Cloud returns a `device_id`, short-lived access credential, rotating refresh credential, and initial control snapshot. Setup/Repair stores the secret credentials only in Keychain and starts/restarts the Agent.
8. Agent Core reads Keychain references, reports authenticated presence, and applies the received desired configuration. Missing/revoked credentials return it to `unpaired` and disable sensitive Collectors.

The listener accepts exactly one successful callback, has no CORS policy, does not expose a general loopback API, and is closed after success, expiry, cancellation, or any terminal validation failure. A replayed code, stale session, mismatched state, mismatched PKCE proof, non-loopback callback, or attempted second callback fails closed. Setup/Repair logs only a redacted result and request correlation ID.

### 4.2 Credentials, revocation, and control polling

- Device key material and access/refresh credentials reside only in Keychain. SQLite stores non-secret credential references, device identity, and token validation state; Event payloads, diagnostics, and ordinary logs contain neither secrets nor authorization codes.
- Cloud records the device public-key fingerprint, Workspace, platform, architecture, Agent version, credential generation, pairing time, and revocation state.
- Agent sends an authenticated heartbeat/control request every 30 seconds while healthy. The response returns revocation state, monotonically increasing desired Collector revisions, and the minimal current configuration. Transient failure uses bounded exponential retry capped at five minutes; it never blocks local startup or creates an unbounded queue.
- A revoked response or refresh failure that Cloud declares non-retryable immediately removes usable local pairing state, disables sensitive Collectors, and requires Setup/Repair to pair again. Cloud outage alone does not remove a valid pairing or stop already-authorized local collection.

### 4.3 Owner Workspace authorization policy

For this explicitly private, self-use channel, an authenticated Owner enabling or disabling Network collection in the paired device's Workspace is the product-level authorization. Cloud writes an immutable audit record with actor, device, old/new value, revision, and time; remote disable takes effect on the next control response.

This is deliberately narrower than a general remote-control capability:

- it applies only to configuration of the paired owner's Network Collector;
- it cannot bypass macOS Location Services/TCC, and it grants no screen, microphone, camera, remote desktop, or command authority;
- a device accepts only the configuration for its own Workspace and only a complete newer revision;
- the Dashboard still cannot read unuploaded local data or connect directly to the Agent.

## 5. S2B Network Collector

### 5.1 Source and schedule

PlatformBridge uses `NWPathMonitor`, CoreWLAN, and active-interface address APIs to provide:

- active interface class: `wifi`, `wired`, `other`, or `none`;
- SSID and canonical upper-case colon-separated BSSID when the active path is Wi-Fi and macOS supplies them;
- one normalized active-interface local IPv4 and one IPv6 address when available.

Rust requests an immediate observation after the Collector reaches `initializing`; it requests a confirmation on a PlatformBridge network-path change and every 60 seconds. The 60-second timer uses skipped missed ticks. Sleep, suspension, reconnect, or delayed scheduling never creates catch-up samples.

The Bridge contract is additive: Agent Core owns all timer, event, deduplication, and health semantics. It carries platform observations only. Any incompatible future Bridge field changes must follow the existing protocol-version compatibility rule.

### 5.2 Permission and health states

- No valid paired identity or `network.enabled=false`: `disabled`; no network Event is generated.
- An enabled Collector with an available observation: `running`.
- A Wi-Fi path where macOS withholds SSID/BSSID, including because Location Services is unavailable: `degraded`, but the interface type and local IP fields continue when available.
- A wired or non-Wi-Fi path is not degraded merely because Wi-Fi identity is null.
- Bridge unavailability, invalid data, or DbActor failure follows the common bounded Collector retry/error mapping. No component prompts for macOS permission automatically.

There is no retroactive collection after a user grants the system permission. The next regular or path-change observation becomes the new baseline.

### 5.3 Event contract

Only a changed normalized observation emits `network.changed` schema version `1`.

```json
{
  "interface_type": "wifi",
  "wifi_identity_available": true,
  "ssid": "Example Wi-Fi",
  "bssid": "AA:BB:CC:DD:EE:FF",
  "local_ipv4": "192.168.1.25",
  "local_ipv6": "2001:db8::25"
}
```

Rules:

- `interface_type` is required. SSID, BSSID, IPv4, and IPv6 are nullable and omitted neither as empty strings nor placeholders.
- SSID is normalized to NFC Unicode; BSSID is canonicalized before comparison; IP values are parsed and canonicalized. Link-local, loopback, and unusable addresses are not emitted as the active address.
- The Event envelope uses `source=network`, `sensitivity=high`, no attachments, Agent-created UUID identity, and its completed-observation time as `occurred_at`.
- The Event contains no public egress IP, gateway, traffic counter, reachability result, latency, coordinate, or external geo response.
- Exact normalized fact changes create one Event. Duplicate path notifications and the reconciliation timer create none.

### 5.4 Durable state and backpressure

S2B adds an immutable local migration for a single `network_observation_state` row per Collector containing the normalized latest fact, its observation time, and configuration revision. Its sensitivity and lifecycle match raw network data.

For a normal changed observation, DbActor commits in one SQLite transaction:

1. the immutable Event;
2. its stable Outbox row; and
3. the updated network checkpoint plus Collector state.

After a crash, the committed checkpoint suppresses duplicates. A checkpoint older than 30 days is discarded before use; the next successful observation is a fresh initial fact. Disabling or unpairing clears the checkpoint in the same control-state transition. There is no offline historical reconstruction.

Network follows the existing global Outbox high/low-water policy. While backpressure is active, it keeps only the newest normalized fact in the checkpoint and creates no new network Event or in-memory backlog. When the low-water condition recovers, it emits one current fact if it differs from the most recently committed network Event. Already committed immutable Events are never rewritten or silently deleted to coalesce them.

## 6. S3 Cloud ingestion, location projection, and retention

### 6.1 Ingestion and data model

S3 uses the normal authenticated Event Sync path. At the API boundary—not in the Agent—the service records the request's trusted remote address as the observed public egress IP. Proxy handling must use a fixed, trusted-proxy configuration; forwarded headers from arbitrary clients are never trusted.

The Cloud schema adds, at minimum:

- `devices` and credential-generation/revocation records;
- one-time pairing-session/code records with expiry and consumed state;
- `collector_configs` with device/Workspace scope, desired revision, and normalized Network settings;
- append-only `collector_config_audit` records;
- `device_heartbeats` or equivalent presence projection;
- `network_raw_observations` keyed to the accepted Event and holding SSID, BSSID, local IP values, API-observed public IP, capture time, and raw-expiry time;
- `network_location_projections` holding country, region, city, accuracy class, inferred time, and enrichment rule/provider version.

All device- and observation-bearing queries enforce Workspace scope. `network_raw_observations` is an ingestion projection, not a second client-authoritative Event stream.

### 6.2 Geo enrichment boundary

A Cloud Worker consumes accepted network observations through a `GeoEnrichmentPort`. Agent Core and PlatformBridge never import a geo-provider SDK or call a geo API.

The adapter can use the observed public IP and Wi-Fi identity data to infer only country/region/city plus an accuracy class. It may discard a provider-returned coordinate; no precise coordinate is persisted or shown. A provider is intentionally not selected in this design; selecting one requires a separate supplier/privacy review and must state which inputs leave Cloud. SSID is retained by Cloud as approved raw data, but is not sent to a provider unless that later review proves it necessary.

### 6.3 Retention and deletion

- Raw SSID, BSSID, local IPv4/IPv6, and Cloud-observed public IP have a 30-day retention period in both Cloud raw-observation storage and Local Event/checkpoint storage.
- A daily retention job deletes expired raw rows and the source Event payload that contains them, then verifies that dependent search/index records no longer retain the raw fields. It preserves the country/region/city projection and a non-sensitive retention audit fact.
- Coarse location projections may be retained long-term. They have no coordinates or raw IP/Wi-Fi identifiers.
- Disabling the Collector stops future collection; standard retention still removes previously collected raw facts. A future explicit user deletion uses the existing tombstone path and removes both raw observations and projections according to the delete request.

## 7. Required tests and acceptance criteria

### S1B

- only Setup/Repair opens the browser; `agentd` has no browser-launch path;
- localhost-only callback, five-minute expiry, `state`/PKCE mismatch, replay, second callback, and revoked code all fail closed;
- credentials appear only in Keychain; SQLite, Event fixtures, logs, and diagnostics contain no secret;
- Cloud enforces Workspace isolation, credential generation, revocation, and configuration audit;
- valid pairing produces an authenticated heartbeat and applies only a newer complete configuration revision;
- Cloud outage preserves local runtime; confirmed revocation returns sensitive Collectors to `disabled`.

### S2B

- initial observation, path change, and 60-second confirmation produce only real deltas; sleep/restart causes no catch-up burst;
- Wi-Fi identity unavailable causes `degraded` without losing available interface/IP facts; a wired path does not falsely degrade;
- strict contract fixtures reject unknown fields, invalid BSSID/IP values, and forbidden public-IP/geo/traffic fields;
- Event, Outbox, checkpoint, and Collector state commit atomically or not at all; restart deduplicates;
- high-water backpressure creates no unbounded queue and later emits only the current suppressed fact;
- no Network Collector component can link a Cloud client or geo-provider adapter.

### S3

- API uses the trusted request remote address only; user-supplied forwarded IP is ignored;
- raw data expires at 30 days while a coordinate-free coarse projection remains;
- a fake geo adapter proves IP/Wi-Fi inputs are Cloud-only and output is limited to country/region/city plus accuracy;
- Dashboard queries cannot cross Workspaces and distinguish unavailable/degraded network identity from a false “no network” state.

## 8. Expected change surface for later implementation

Implementation plans may touch only the required Cloud API/Web/Worker packages, Keychain and Setup/Repair code, Bridge contract/PlatformBridge network source, Agent Core/Domain/DbActor/Sync contracts, immutable local and Cloud migrations, data dictionary/ADR/task documentation, and their contract/integration/process tests. No Battery Collector, traffic monitor, local dashboard, or generic remote-control abstraction is in scope.

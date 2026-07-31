# ADR-0006: Automatic Owner Pairing and Network Collector Authorization

Status: Accepted
Date: 2026-07-31

## Context

The product specification's original S1B pairing text uses a manually entered
eight-character pairing code and says Web configuration cannot expand local
authorization. The self-use product decision is different: pairing should be
automatic after the owner opens the Setup App, and the authenticated owner
should enable or disable the paired device's Network Collector from the Cloud
Dashboard. Network data is sensitive because it includes SSID, BSSID, and IP
addresses, but it is needed for later Cloud-only coarse location inference.

## Decision

- Setup/Repair, never `agentd`, opens the system browser for a one-time
  loopback callback pairing session. The callback uses localhost, short TTL,
  state, PKCE, and an authorization code exchange; it never carries a
  long-lived credential in the URL.
- Device credentials are held only in Keychain. The Agent uses them for
  authenticated heartbeat and control configuration, not for S1B Event sync.
- For a private Owner Workspace, an audited owner configuration revision is
  the product-level authorization to enable or disable Network collection on
  a device already paired to that Workspace.
- This exception is limited to Network Collector configuration. It does not
  bypass macOS Location Services/TCC, create broad remote-control authority,
  or allow the Dashboard to access unuploaded local data.
- Raw SSID, BSSID, local IP, and Cloud-observed public IP are retained for 30
  days. Country/region/city-level, coordinate-free projections may be
  retained long-term. Geo enrichment runs only in Cloud through a Port.

## Consequences

- S1B must implement a small, hardened Setup-only loopback listener despite
  the general prohibition on a local loopback HTTP API. It has no general API
  surface and is closed outside the pairing transaction.
- The original manual-code-only text in product-spec §6.3 is superseded for
  this private channel. A public distribution channel requires a new ADR and
  separate consent review before reusing this policy.
- Cloud configuration requires Workspace scope checks, complete monotonic
  revisions, and immutable actor/time/old/new-value audit records.
- Network collection remains unavailable without both a valid pairing and the
  Cloud configuration. Lack of OS permission degrades data availability but
  does not prompt or bypass the operating system.

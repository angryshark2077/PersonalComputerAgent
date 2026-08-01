# S1B Final Acceptance Repair Design

**Status:** approved design

## Problem

The final review of the S1B deployment-acceptance correction found two
remaining false proofs in its local synthetic process harness:

1. The callback state was reused as the PKCE verifier, so the harness did not
   prove that the Agent generated and retained PKCE proof material.
2. The message canary existed only in a response header, and SQLite artifacts
   were inspected only after an Agent helper checkpointed its WAL. A body leak
   or transient paired-state WAL leak could therefore escape observation.

## Scope

- Give the test Agent a generated, opaque PKCE verifier before pairing starts.
  It computes the challenge, keeps the verifier out of callback metadata, and
  submits it only to the shared Cloud exchange endpoint.
- Let the shared Cloud start pairing with that challenge and continue to own
  the session, callback state/code, credential issuance, device state, audit,
  configuration, and revocation.
- Put the runtime-generated message canary into an observed non-credential
  JSON response body. It must remain absent from all persisted or emitted
  Agent/Dashboard/Cloud artifacts.
- Add a checkpoint-free paired-state inspection boundary: the Agent helper
  scans its SQLite main/WAL/SHM bytes and credential-double absence before it
  calls its final checkpoint or exits. The parent script retains its final
  artifact scan after revocation.

## Data flow

```text
Agent-generated verifier -> SHA-256 challenge -> Cloud pairing session
callback state/code -> Agent-held verifier + code -> Cloud exchange
-> Agent paired control -> Dashboard configuration/revoke -> Agent revoked control
```

The verifier is test-only synthetic sensitive material. It is never passed as
Agent stdin callback metadata, returned by the Cloud, emitted to stdout/stderr
or JSON status, or written to SQLite/WAL/SHM. The callback state remains a
distinct opaque value. The exchange succeeds only because the submitted
verifier hashes to the challenge recorded in the shared Cloud session.

## Assertions

The repaired acceptance test proves:

1. Pairing session creation receives a challenge derived from a verifier that
   the Agent helper later uses for exchange; callback state is not accepted as
   the verifier.
2. A callback still yields exactly one Cloud-issued credential and the
   existing shared configuration/revocation path remains unchanged.
3. The message canary is present in at least one observed non-credential JSON
   response body, while all sensitive values remain absent from process
   streams, status JSON, SQLite/WAL/SHM, credential-double artifacts, and
   fixture source.
4. Paired-state SQLite/WAL/SHM scanning runs before the helper's checkpoint,
   so transient storage cannot be hidden by checkpoint truncation.

## Exclusions

- No production pairing protocol change, Railway deployment, secret/domain/
  variable action, database migration, Keychain ACL work, or Setup IPC change.
- No change to the already-approved Dashboard private-proxy correction.
- This remains local synthetic acceptance, not proof of Railway, TLS,
  PostgreSQL, signed Setup transport, or macOS Keychain behavior.

## Verification

Run the focused shared process acceptance and Agent control-process tests,
then the existing full local gate with the explicit test Railway-private
origin. The release remains blocked unless the repaired process test and full
gate both pass without canary leakage.

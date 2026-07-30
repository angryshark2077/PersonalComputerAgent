Evaluate Beta readiness using `ACCEPTANCE.md`, `SECURITY.md`, `PERFORMANCE.md` and S6.

A release is blocked by any unverified:
- Keychain secret path
- permission revoke behavior
- cross-workspace isolation
- tombstone resurrection
- migration recovery
- update signature/notarization
- WeChat normal-path no-interference
- crash/outbox durability

Return PASS/FAIL per gate with commands and artifacts. Never infer PASS.

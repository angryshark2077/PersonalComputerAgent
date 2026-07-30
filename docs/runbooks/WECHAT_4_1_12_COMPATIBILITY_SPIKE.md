# WeChat 4.1.12 Compatibility Spike

## Goal

Determine capability, not force compatibility. The result must be one of:

```text
supported
partially_supported
unsupported
inconclusive
```

## Safety constraints

- Use a test device/account owned and authorized by the developer.
- Read-only.
- Do not kill/open/re-sign WeChat.
- Do not bypass SIP/TCC.
- Do not publish offsets, keys or private fixtures.
- Store KeyMaterial in temporary Keychain item, then remove.
- Do not copy full plaintext DB to public `/tmp`.

## Checks

1. Detect process and app version.
2. Discover account directories.
3. Read DB salts.
4. Passive scan capability probe.
5. Validate candidate KeyMaterial against:
   - `session.db`
   - `contact.db`
   - at least one message shard
6. Verify SQLCipher parameters:
   - page size
   - KDF rounds
   - HMAC/reserve
7. Verify schema:
   - Session table and sort timestamp
   - Contact lookup
   - Message shard discovery
   - message ID / sort_seq / create_time
8. Verify WAL refresh.
9. Verify per-talker incremental query.
10. Verify duplicate and gap handling.

## Required evidence

- sanitized version/capability report
- fixture schema hashes
- exact error code on failure
- no keys, message body, wxid or absolute user paths
- ADR if parameters/schema require change

## Exit gate

Do not mark supported because one database opens. All required DB and cursor paths must pass.

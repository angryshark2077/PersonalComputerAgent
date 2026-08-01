# ADR-0003: Silent WeChat Provider after Explicit Product Authorization

Status: Accepted  
Date: 2026-07-30

## Decision

Normal Provider behavior:

1. Wait silently when WeChat is absent or not logged in.
2. Reuse verified KeyMaterial from macOS Keychain.
3. If missing/invalid, passively scan the currently logged-in WeChat process only when capability probe allows.
4. Verify SQLCipher databases read-only.
5. Monitor session DB/WAL and pull real messages using per-talker sort_seq cursor.
6. Persist messages before advancing cursor.

## Prohibited normal-path actions

- kill/open WeChat
- re-sign WeChat
- prompt login
- LLDB Active Extraction
- store KeyMaterial in files/SQLite/logs
- send or modify messages

Active Extraction is an explicit Repair/Developer capability only.

## Stored-key SQLCipher probe boundary

The first production source capability is Apple-only and passive. It may read only the fixed
`keychain://com.pca.wechat/current-v1` item and an adapter-selected database path. It must not
discover or open a real account path until the later source-discovery task wires an explicitly
authorized target.

Before the Provider may return any source record, one bounded probe must establish all of the
following:

1. the fixed Keychain item contains a versioned, account-scoped 32-byte raw key;
2. the native library reports SQLCipher capability;
3. the database opens with `SQLITE_OPEN_READ_ONLY` and accepts the key;
4. source version and schema are within the adapter's explicit compatibility range; and
5. the database account proof exactly matches the Keychain account proof.

The probe has a two-second deadline, uses SQLite `query_only` plus an authorizer that permits only
`SELECT` and `READ`, and accepts only bounded single-`SELECT` proof queries. Failure clears prior
proof and returns an explicit redacted state: `WECHAT_WAITING_SOURCE`,
`WECHAT_CAPABILITY_UNAVAILABLE`, `WECHAT_DATABASE_UNAVAILABLE`, `WECHAT_KEY_REJECTED`,
`WECHAT_PROBE_TIMEOUT`, `WECHAT_UNSUPPORTED_SOURCE_VERSION`, `WECHAT_UNSUPPORTED_SCHEMA`, or
`WECHAT_ACCOUNT_UNVERIFIED`. Errors and ordinary diagnostics never include KeyMaterial, account
proofs, message bodies, conversation names, or source paths.

## Native dependency decision

The Owner approved `rusqlite 0.32.1` with `bundled-sqlcipher-vendored-openssl`. Cargo feature
unification means workspace builds use `libsqlite3-sys 0.30.1` with bundled SQLCipher 4.5.7 and
vendored OpenSSL for both this Provider and existing `pca-db-local` consumers. This adds native C
and cryptographic compilation, binary/SBOM size, and vulnerability patching responsibilities.
Licenses are MIT for `rusqlite`, `libsqlite3-sys`, and `openssl-sys`; the bundled SQLCipher source
uses the Zetetic BSD 3-Clause license; `openssl-src` is dual MIT/Apache-2.0. Local database
migration, WAL, locking, outbox, and integrity regressions are therefore required in workspace
verification.

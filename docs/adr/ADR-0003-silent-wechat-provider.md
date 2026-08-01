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

The fixed WeChat Keychain read uses an explicit `SecItemCopyMatching` search with
`kSecUseAuthenticationUI = kSecUseAuthenticationUISkip`; an ACL/authentication requirement
therefore fails closed without showing macOS authentication UI and no database probe starts.

The probe has one two-second wall-clock budget beginning before the Keychain read. One source-owned
worker performs the complete Keychain, snapshot, and SQLCipher operation. The caller waits only on
the coordinator's bounded result channel and returns `WECHAT_PROBE_TIMEOUT` at its deadline even if
the worker is blocked in an operating-system call. Coordinator generations exclusively control
proof: a timeout or newer probe invalidates the in-flight generation, and a late result can never
restore success. The source path must resolve to a regular file on a local, non-FUSE filesystem.
Before SQLite opens anything, the worker copies only the source DB and optional WAL through
read-only, `O_NOFOLLOW` file descriptors into a private temporary directory. SQLite opens only that
private snapshot, so its WAL VFS may create or update SHM only outside the WeChat source directory.
Source DB/WAL metadata is checked before and after copying, and any concurrent source change fails
closed.

SQLite uses `query_only` plus an authorizer that permits only `SELECT` and `READ`, and accepts only
bounded single-`SELECT` proof queries. Failure clears prior proof and returns an explicit redacted
state: `WECHAT_WAITING_SOURCE`,
`WECHAT_CAPABILITY_UNAVAILABLE`, `WECHAT_DATABASE_UNAVAILABLE`, `WECHAT_KEY_REJECTED`,
`WECHAT_PROBE_TIMEOUT`, `WECHAT_UNSUPPORTED_SOURCE_VERSION`, `WECHAT_UNSUPPORTED_SCHEMA`, or
`WECHAT_ACCOUNT_UNVERIFIED`. Errors and ordinary diagnostics never include KeyMaterial, account
proofs, message bodies, conversation names, or source paths.

macOS does not provide safe cancellation for an individual in-flight local filesystem syscall. A
permanently stuck worker therefore quarantines that source until the Agent process restarts. While
it is stuck, later probes fail closed without queueing another job or spawning another thread. The
request and result channels each have capacity one, and the source never replaces its worker, so
retries cannot cause unbounded thread, task, or queue growth. Dropping the source closes its
channels; a worker that eventually returns exits, while a permanently blocked syscall is reclaimed
by process restart.

## Native dependency decision

The Owner approved exactly `rusqlite =0.32.1` with `bundled-sqlcipher-vendored-openssl`. Cargo feature
unification means workspace builds use `libsqlite3-sys 0.30.1` with bundled SQLCipher 4.5.7 and
vendored OpenSSL for both this Provider and existing `pca-db-local` consumers. This adds native C
and cryptographic compilation, binary/SBOM size, and vulnerability patching responsibilities.
Licenses are MIT for `rusqlite`, `libsqlite3-sys`, and `openssl-sys`; the bundled SQLCipher source
uses the Zetetic BSD 3-Clause license; `openssl-src` is dual MIT/Apache-2.0. Local database
migration, WAL, locking, outbox, and integrity regressions are therefore required in workspace
verification.

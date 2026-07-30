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

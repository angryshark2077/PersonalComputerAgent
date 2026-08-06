# ADR-0009: Preauthorized One-Time WeChat Key Recovery

Status: Accepted
Date: 2026-08-06

## Context

A fresh device has no validated `com.pca.wechat/current-v1` Keychain material. Passive Provider
discovery cannot recover the reviewed WeChat 4.1.12 WCDB credential. Asking for administrator
authorization after WeChat opens breaks unattended initialization, while copying, re-signing,
quitting, or relaunching WeChat is unacceptable.

## Decision

Fresh setup may run one explicit Repair preparation after installation, system authorization, and
owner pairing:

1. Setup verifies that SIP is already disabled, the official WeChat bundle matches a reviewed
   binary hash, LLDB is available, and the fixed Keychain placeholder exists.
2. Setup requests administrator authorization once while WeChat may remain closed.
3. A detached, time-bounded Repair worker waits for the next official WeChat launch. This is not an
   Agent or CommunicationProvider task.
4. The privileged debugger loads only the reviewed breakpoint before waiting. It never launches,
   quits, copies, re-signs, or modifies WeChat.
5. The user-level signed Repair worker receives one 32-byte candidate through a private Unix-domain
   datagram, validates every required SQLCipher database read-only, and only then updates the fixed
   Keychain item.
6. The debugger detaches and the privileged process, user worker, and private temporary directory
   exit or are removed after success or the bounded waiting period.

After the setup authorization succeeds, launching and logging in to WeChat must not display another
administrator, Keychain, or PCA prompt. A rejected setup authorization may be retried from Setup;
there is no manual “Recover WeChat Key” action in the normal installed UI.

## Boundaries

- `agentd` and the normal WeChat Provider must never invoke LLDB or elevate privileges.
- Existing validated Keychain material skips this Repair preparation.
- Only explicitly reviewed version plus dylib-hash profiles may run.
- SIP is never changed by PCA. The owner disables it before setup and re-enables it after recovery.
- Key material is never written to arguments, files, SQLite, stdout, stderr, or logs.
- Failure leaves other PCA collectors and synchronization running.

This ADR narrows ADR-0003 only for the explicitly authorized, one-time Setup/Repair lifecycle. Its
prohibitions remain unchanged for the normal Provider path.

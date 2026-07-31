# ADR-0007: Owner-Authorized WeChat Outbound Recording

Status: Accepted
Date: 2026-07-31

## Context

The private self-use channel already permits an authenticated Owner Workspace
to configure the paired device's Network Collector through ADR-0006. The owner
now requires a narrower Communication capability: preserve only final,
successfully sent WeChat text and its conversation context. A physical-key
collector would capture IME composition, drafts, and unrelated applications,
and cannot establish whether a message was sent.

## Decision

- The product does not add a Keylogger. It records only source-confirmed
  outgoing WeChat text through the existing read-only Communication Provider
  architecture.
- Owner Workspace configuration is product-level authorization only for the
  exact `communication.wechat` scope `outgoing + text + full-sync`; it is
  independently auditable and does not broaden ADR-0006's Network scope.
- The message must be proven local-account, outgoing, text, non-empty, and
  successfully sent before it becomes an Event. Ambiguous source records fail
  closed.
- Text and conversation display names are `high` sensitivity and retained
  locally and in Cloud for 90 days. WeChat recalls do not remove previously
  accepted PCA records.
- The decision does not authorize Telegram, ChatGPT, raw keystrokes, UI
  scraping, incoming messages, or WeChat modification/injection/login.

## Consequences

- The existing V0 ban on keylogging remains in force; this is a source-based
  communication Provider, not an input-capture exception.
- The WeChat Adapter must maintain strict version/schema capability probes and
  must stop rather than guess whether a message was successfully sent.
- S1B must add an audited, monotonic configuration revision for this exact
  scope before local collection can run in production.
- The Event/Outbox/Cursor transaction, Workspace enforcement, retention, and
  diagnostics rules require contract and failure-path tests before release.

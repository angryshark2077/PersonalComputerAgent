# WeChat Outbound Message Collector Design

**Date:** 2026-07-31
**Status:** Approved for implementation planning
**Prerequisite:** S1B automatic pairing and Cloud control configuration.

## 1. Objective

Add one narrow Communication Provider slice that records and syncs only the
device owner's WeChat messages after the WeChat data source confirms they were
successfully sent. This is not a Keyboard Collector and must not observe raw
keystrokes, IME composition, drafts, or other applications.

For example, the recorded body is the source-confirmed final text `你好`, never
the physical-input sequence `nihao`.

## 2. Approved scope

### Included

- One `communication.wechat` Rust Provider using the product's existing
  read-only WeChat SQLCipher/WAL/Cursor architecture.
- Source-confirmed outgoing, text-only messages from the device owner's
  account.
- Stable conversation ID, display name, direct/group type, sent time, and
  final text body.
- Standard Event, local message projection, transactional Outbox, and later
  authenticated Cloud Sync.
- A private Owner Workspace may enable or disable this precise Collector
  scope in the Cloud Dashboard; every change is audited.
- Local and Cloud retention of body and conversation display name for 90 days.

### Excluded

- All global keyboard hooks, key codes, IME composition/pre-edit text,
  clipboard, notifications, screen scraping, Accessibility UI scraping, and
  any text from non-WeChat applications.
- Incoming messages, drafts, failed sends, edits, images, voice, files,
  attachments, contact profiles, group-member lists, and message reactions.
- Telegram and ChatGPT adapters.
- Opening, killing, signing, re-signing, injecting into, logging into, or
  otherwise modifying WeChat; automatic Active Extraction remains prohibited.
- Automatic deletion in PCA when the owner recalls a WeChat message.

## 3. Architecture and data flow

```text
WeChat message database/WAL (read-only)
  -> WeChatProvider source adapter
  -> outgoing-text eligibility gate
  -> Agent Core Event factory
  -> DbActor transaction: Event + messages projection + Outbox + Cursor
  -> Sync Engine (after S1B) -> Cloud API -> Dashboard
```

`WechatProvider` implements the existing `CommunicationProvider` port. The
Provider is the only component that knows WeChat's source schema; Agent Core
owns identity, Collector configuration, Event IDs, persistence, retry, and
sync. No Provider or Collector imports a Cloud client. Swift has no role in
message reading.

The Provider may emit a message only when the source proves all of the
following:

1. the record belongs to the local WeChat account;
2. it is an outgoing record;
3. it is a text record with a non-empty final body; and
4. its durable source state represents a successful send.

The Adapter Contract must expose those facts explicitly. If a supported source
version cannot establish any one of them, the Provider fails closed and emits
no message. It must never infer success from a typed key, a session summary,
an unread counter, or a UI change.

## 4. Contracts and persistence

Each eligible row produces `communication.message_sent`, schema version `1`:

```json
{
  "adapter_key": "wechat",
  "external_message_id": "provider-stable-id",
  "external_conversation_id": "provider-stable-conversation-id",
  "conversation_display_name": "Example Group",
  "conversation_type": "group",
  "message_type": "text",
  "body": "你好",
  "sent_at_ms": 1780000000000,
  "direction": "outgoing"
}
```

The Event uses `source=communication.wechat`, `sensitivity=high`, no
attachments, an Agent-generated UUID, and a stable idempotency key derived
from `account_id + talker + server_id/sort_seq`. The strict schema rejects
unknown fields, empty bodies, incoming/unknown direction, non-text types, and
attachments.

For every new message, DbActor commits in one SQLite transaction:

1. immutable Event;
2. local conversation/message projection;
3. one stable Sync Outbox row; and
4. the per-account/per-talker Cursor advancement.

The unique source key prevents duplicate Events through repeated WAL notices,
crashes, retries, and restart. A message recall does not alter an already
accepted PCA Event or projection, by explicit product decision. Retention and
an explicit PCA delete request remain the only deletion paths.

## 5. Authorization, privacy, and retention

The Provider remains `disabled` until the device has a valid S1B pairing and
its own Workspace's complete, newer `communication.wechat` configuration is
enabled for this exact outbound-text/full-sync scope. The authenticated Owner
Workspace configuration is product-level authorization for this self-use
channel; it must record actor, device, old/new scope, revision, and time.
This authorization cannot grant operating-system privileges, access another
Workspace, or enable any other Communication data.

WeChat KeyMaterial and device credentials stay only in Keychain. SQLite holds
credential references and non-secret state; normal logs, error messages, and
diagnostic exports contain no body, conversation display name, or key
material. The normal provider does not prompt for login or Full Disk Access.
If the source cannot be read with already-granted system access, it reports
the appropriate capability state and waits.

The Local retention job and Cloud retention worker remove message body and
conversation display name after 90 days, including search-index copies. A
non-sensitive audit/tombstone fact may remain. A Cloud configuration disable
stops future reads but does not retroactively delete prior messages.

## 6. Lifecycle, failure, and recovery

- No paired identity or disabled config: `disabled`; do not probe, scan, or
  read WeChat.
- WeChat absent/not logged in: `waiting_source`; low-frequency detection only.
- Key, database, schema, permission, or eligibility-evidence failure:
  `capability_unavailable`, `unsupported`, or `degraded` according to the
  existing Provider error map; no message is fabricated.
- On sleep, wake, or restart, wait for the source database/WAL to stabilize,
  then resume from durable Cursors. Only rows eventually committed by WeChat's
  source can be caught up.
- Cloud unavailability does not stop source reading: accepted messages commit
  to the persistent Outbox. Sync retries remain bounded.
- At global Outbox backpressure, stop incremental source advancement and keep
  the durable Cursor at its last committed value. Do not retain message bodies
  in an in-memory queue. Resume from that Cursor after the low-water condition.

## 7. Required tests and acceptance criteria

- Contract fixtures prove an outgoing source row containing `你好` emits that
  final body; no fixture exposes keystrokes or IME pre-edit text.
- Incoming, draft, failed-send, unknown-success, empty, image, voice, file,
  attachment, and malformed records produce no Event or projection.
- Valid records preserve the allowed conversation context and generate a
  `high`-sensitivity, text-only Event with no attachment references.
- Repeated WAL signals, duplicate source rows, process crash at every
  transaction boundary, restart, and catch-up all produce exactly one Event,
  one Outbox row, and one projection per source key.
- Unpaired/config-disabled paths do not probe or read WeChat. Unsupported
  versions and unavailable system access fail closed without starting,
  prompting, killing, or modifying WeChat.
- Backpressure causes neither in-memory body accumulation nor Cursor loss;
  recovery replays committed source records exactly once.
- Cloud tests prove Workspace isolation, Owner audit records, 90-day Local and
  Cloud expiry of body/display name/search copies, and no body/key in logs or
  diagnostics.

## 8. Delivery sequence and change boundary

1. Implement and verify S1B pairing, authenticated control polling, and
   audited `communication.wechat` configuration first.
2. Add the local WeChat outgoing-text Provider slice with source fixtures,
   Event/Projection/Outbox/Cursor transaction, and debug-only identity tests.
3. Add authenticated Sync and Cloud message/conversation projections, then
   enforce the 90-day worker cleanup and Dashboard query scope.

Expected implementation changes are limited to the existing Keychain,
Provider-contract, WeChat source-adapter, Domain/Event contract, Agent Core,
DbActor/immutable migrations, Sync, Cloud API/Worker/Web projections, data
dictionary, ADR/task documentation, and relevant fixture/unit/integration/
process tests. No generic keylogger, Telegram, ChatGPT, or macOS Bridge
keyboard capability is in scope.

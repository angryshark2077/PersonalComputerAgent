# WeChat 双向多媒体采集实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在已配对 macOS 设备上安全采集允许 WeChat 会话的双向文本、语音、图片和视频，并通过私有 R2 在 Dashboard 查看，统一保留 180 天。

**Architecture:** `WechatProvider` 是唯一读取 WeChat 私有本地来源的 Rust 组件；它只产生经过资格校验的标准 Event 与附件 manifest。Agent Core 与 `DbActor` 将 Event、消息投影、spool、Outbox 和 Cursor 原子落盘。Cloud 的 Communication API 单独接收高敏感事件并协调私有 R2；Web 只查询 Owner 已授权范围并请求短效签名 URL。

**Tech Stack:** Rust stable/Tokio/rusqlite；TypeScript/Hono/Drizzle/Postgres；Cloudflare R2 的 S3 API；Next.js；现有 macOS Keychain。

## Global Constraints

- 仅收发文本、语音、图片、视频；仅 direct 与可靠证实成员数 `<= 8` 的 group。
- WeChat 未配对、配置禁用、Key/Schema/群人数不可证明时 fail closed；不得使用键盘、通知、UI/Accessibility 抓取或启动、修改 WeChat。
- Provider 不调用 Cloud；Swift Bridge 不接触 WeChat、SQLite、R2、同步或保留策略。
- Event、投影、Outbox、Cursor 与附件 spool 引用必须在同一 SQLite 事务提交；未知字段一律拒绝。
- R2 Bucket 私有、无 public URL；Postgres 不存二进制媒体；KeyMaterial、R2 secret、正文、会话名、路径不进普通日志或诊断。
- 本地、Cloud 投影和 R2 原件统一 180 天；删除使用 Tombstone，离线设备不得复活已删除内容。
- 不修改 Railway 既有变量、密钥、数据库变量或公网域名。R2 Bucket、凭据和新配置仅在最终部署阶段由 Owner 提供。

## File Structure

- `crates/wechat-provider/`：只读 WeChat source adapter、资格门、媒体 manifest 与状态机；不依赖 Cloud。
- `crates/provider-contracts/`：版本化的 Provider DTO、消息方向/类型/会话资格与统一错误状态。
- `crates/domain/`：严格 Communication Event 与 attachment manifest 的纯领域校验。
- `crates/db-local/`：messages、conversations、cursors、attachment spool、tombstones 的 migration、repository 与 DbActor 请求。
- `agent/core/`：配置 v2 应用、Provider supervisor、Communication Outbox 与 R2 上传协调。
- `contracts/`、`packages/contracts/`：控制面 v2、Communication sync、object prepare/complete 的 JSON Schema 和 fixtures。
- `packages/db-cloud/`、`apps/cloud-api/`：Cloud 投影、设备鉴权、R2 adapter、短效 URL、Owner 查询。
- `apps/cloud-worker/`：保留期/Tombstone/R2 清理 Job；不得放进请求处理器。
- `apps/web-dashboard/`：配置说明、Provider 健康、会话、消息和媒体预览。

---

### Task 1: 锁定 v2 授权合同与纯领域 DTO

**Files:**
- Modify: `contracts/agent-control-snapshot.schema.json`
- Modify: `contracts/dashboard-control.schema.json`
- Modify: `packages/contracts/agent-control-snapshot.schema.json`
- Modify: `packages/contracts/dashboard-control.schema.json`
- Create: `contracts/communication-message-recorded.schema.json`
- Create: `contracts/communication-object.schema.json`
- Create: matching `packages/contracts/` schemas and valid/invalid fixtures
- Modify: `packages/contracts/src/types.ts`
- Modify: `packages/contracts/tests/contracts.test.ts`
- Modify: `crates/domain/src/lib.rs`
- Test: `crates/domain/tests/communication_event.rs`

**Consumes:** S1B control revision and the approved exact v2 scope.

**Produces:** `CommunicationScopeV2`, `CommunicationMessageRecorded`, `CommunicationAttachment`, and schemas that reject broader scope.

- [ ] **Step 1: Write failing contract and domain tests**

```rust
#[test]
fn rejects_group_larger_than_eight_and_unknown_attachment_fields() {
    assert!(CommunicationMessageRecorded::try_new(valid_group_message(9)).is_err());
    assert!(serde_json::from_value::<CommunicationAttachment>(json!({
        "attachment_id": "a", "kind": "image", "sha256": "a".repeat(64),
        "size_bytes": 1, "mime_type": "image/png", "extra": true
    })).is_err());
}
```

- [ ] **Step 2: Run the focused tests and confirm RED**

Run: `cargo test -p pca-domain --test communication_event`  
Expected: FAIL because the DTO and validator do not exist.

- [ ] **Step 3: Add the smallest strict types and schemas**

Define `Direction::{Incoming,Outgoing}`, `MessageKind::{Text,Audio,Image,Video}`, and
`ConversationScope::{Direct,Group { member_count: u8 }}`. `try_new` must require non-empty
text for `Text`, at least one complete manifest for media, and `member_count <= 8`. The v2
control schema must accept only both directions, all four types, the exact fixed scope string,
`max_group_members: 8`, `sync_mode: "full"`, and `retention_days: 180`.

- [ ] **Step 4: Run GREEN and static checks**

Run: `cargo test -p pca-domain --test communication_event && pnpm --filter @pca/contracts test`  
Expected: PASS; old outgoing/text-only config fixture is rejected and the v2 fixture is valid.

- [ ] **Step 5: Commit**

```bash
git add crates/domain contracts packages/contracts
git commit -m "feat: define wechat multimedia contracts"
```

### Task 2: Build the read-only Provider contract and fixture adapter

**Files:**
- Create: `crates/wechat-provider/Cargo.toml`
- Create: `crates/wechat-provider/src/lib.rs`
- Create: `crates/wechat-provider/src/source.rs`
- Create: `crates/wechat-provider/src/eligibility.rs`
- Create: `crates/wechat-provider/src/fixtures.rs`
- Create: `crates/wechat-provider/tests/provider_contract.rs`
- Modify: root `Cargo.toml` workspace members
- Modify: `crates/provider-contracts/src/lib.rs`

**Consumes:** Task 1 domain DTO and `CommunicationProvider` boundary.

**Produces:** `WechatSource` port and `WechatProvider::poll_once`, which returns only eligible normalized records and never performs Cloud I/O.

- [ ] **Step 1: Write failing fixture tests**

```rust
#[tokio::test]
async fn emits_only_confirmed_direct_or_small_group_records() {
    let provider = fixture_provider([outgoing_text(), incoming_video(), group_text(9)]);
    let emitted = provider.poll_once().await.unwrap();
    assert_eq!(emitted.len(), 2);
    assert!(emitted.iter().all(|message| message.conversation.is_allowed()));
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test -p pca-wechat-provider --test provider_contract`  
Expected: FAIL because the crate and `poll_once` do not exist.

- [ ] **Step 3: Implement the narrow port and eligibility gate**

```rust
#[async_trait]
pub trait WechatSource: Send + Sync {
    async fn probe(&self) -> Result<SourceCapabilities, DomainError>;
    async fn read_after(&self, cursor: &SourceCursor) -> Result<Vec<SourceRecord>, DomainError>;
}

pub async fn poll_once(&mut self) -> Result<Vec<CommunicationMessageRecorded>, DomainError> {
    self.source.read_after(&self.cursor).await?
        .into_iter().filter_map(eligible_message).collect::<Result<Vec<_>, _>>()
}
```

`eligible_message` must return no record for missing local-account proof, unknown direction,
draft/failed outgoing state, unsupported type, unknown group count, count over eight, incomplete
media, or any unknown source enum. It may log only error codes.

- [ ] **Step 4: Run GREEN**

Run: `cargo test -p pca-wechat-provider --test provider_contract`  
Expected: PASS with direct/small-group accepted and every disallowed fixture omitted.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/provider-contracts crates/wechat-provider
git commit -m "feat: add wechat read-only provider boundary"
```

### Task 3: Add Keychain validation and production source probing

**Files:**
- Modify: `crates/keychain/src/lib.rs`
- Modify: `crates/keychain/src/macos.rs`
- Modify: `crates/wechat-provider/src/source.rs`
- Create: `crates/wechat-provider/src/sqlcipher_source.rs`
- Test: `crates/keychain/tests/wechat_credentials.rs`
- Test: `crates/wechat-provider/tests/sqlcipher_source.rs`
- Modify: `docs/adr/ADR-0003-silent-wechat-provider.md`

**Consumes:** Task 2 `WechatSource` and the existing Keychain credential store.

**Produces:** an Apple-only source implementation that validates a Keychain reference and read-only database capability before returning any source record.

- [ ] **Step 1: Write failing tests for non-interaction and fail-closed probe**

```rust
#[test]
fn missing_key_material_returns_waiting_or_capability_state_without_body() {
    let result = SqlcipherWechatSource::with_test_key_store(EmptyKeyStore).probe_blocking();
    assert_eq!(result.unwrap_err().code, "WECHAT_WAITING_SOURCE");
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test -p pca-wechat-provider --test sqlcipher_source`  
Expected: FAIL because no production source exists.

- [ ] **Step 3: Implement only passive, read-only initialization**

Load KeyMaterial through the existing Keychain port, validate it against a read-only SQLCipher
open, then probe source schema and account identity. Use bounded deadlines and explicit
`WECHAT_*` codes. Do not call shell CLIs, `open`, `kill`, LLDB, Accessibility APIs, or write into
the source directory. Unsupported schema or unavailable metadata returns an error/status and no
source record.

- [ ] **Step 4: Run GREEN and boundary checks**

Run: `cargo test -p pca-keychain --test wechat_credentials && cargo test -p pca-wechat-provider --test sqlcipher_source`  
Expected: PASS; tests prove no credentials are serialized into SQLite-facing DTOs or diagnostics.

- [ ] **Step 5: Commit**

```bash
git add crates/keychain crates/wechat-provider docs/adr/ADR-0003-silent-wechat-provider.md
git commit -m "feat: probe wechat source read-only"
```

### Task 4: Make local messages, cursors, spool and Outbox atomic

**Files:**
- Create: `crates/db-local/migrations/0003_wechat_messages.sql`
- Modify: `crates/db-local/src/repository.rs`
- Modify: `crates/db-local/src/actor.rs`
- Modify: `crates/db-local/tests/runtime_store.rs`
- Create: `crates/db-local/tests/wechat_messages.rs`

**Consumes:** Task 1 messages and attachment manifests.

**Produces:** `DbActorHandle::commit_communication_message`, `load_pending_communication_events`, and `acknowledge_communication_events`.

- [ ] **Step 1: Write the transaction and crash-boundary tests**

```rust
#[tokio::test]
async fn source_key_creates_one_message_outbox_cursor_and_spool_reference() {
    let store = test_store().await;
    store.commit_communication_message(valid_message()).await.unwrap();
    store.commit_communication_message(valid_message()).await.unwrap();
    assert_eq!(store.communication_counts().await.unwrap(), (1, 1, 1, 1));
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test -p pca-db-local --test wechat_messages`  
Expected: FAIL because the migration and actor calls do not exist.

- [ ] **Step 3: Implement migration and one repository transaction**

Create `communication_conversations`, `communication_messages`, `communication_cursors`,
`attachment_spool`, and `local_tombstones`. Add unique `(account_id, external_conversation_id,
source_sequence)` and foreign keys. In one `rusqlite` transaction insert Event, projection,
attachment manifests/spool paths, `sync_outbox`, and cursor; only advance the cursor after all
inserts succeed. Validate spool path is below the app-private spool root, hash is 64 lowercase
hex characters, and size is positive.

- [ ] **Step 4: Run GREEN and migration replay**

Run: `cargo test -p pca-db-local --test wechat_messages && cargo test -p pca-db-local --test runtime_store`  
Expected: PASS, including rollback after injected error and duplicate source signal.

- [ ] **Step 5: Commit**

```bash
git add crates/db-local
git commit -m "feat: persist wechat messages atomically"
```

### Task 5: Wire configuration, Provider lifecycle and bounded local media spool

**Files:**
- Modify: `agent/core/src/cloud_control.rs`
- Modify: `agent/core/src/collector_registry.rs`
- Modify: `agent/core/src/app.rs`
- Create: `agent/core/src/communication.rs`
- Modify: `agent/core/tests/cloud_control_process.rs`
- Create: `agent/core/tests/communication_process.rs`

**Consumes:** Tasks 1–4.

**Produces:** production `communication.wechat` lifecycle tied to paired identity and v2 control revision, with no source probe when disabled.

- [ ] **Step 1: Write failing lifecycle tests**

```rust
#[tokio::test]
async fn disabled_or_unpaired_configuration_never_calls_source_probe() {
    let source = RecordingSource::default();
    run_communication_once(unpaired_control(), source.clone()).await;
    assert_eq!(source.probe_calls(), 0);
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test -p pca-agent-core --test communication_process`  
Expected: FAIL because the supervisor does not exist.

- [ ] **Step 3: Add the smallest supervisor**

Create `CommunicationRuntime` that owns the Provider task and cancels it on revoked identity,
disabled config, or shutdown. It restarts only after bounded backoff for retryable `WECHAT_*`
states. Copy completed source media into the private spool before `DbActor` commit; on quota or
high-water error, do not advance cursor and retain no message body in memory. The supervisor must
not import reqwest or Cloud client code.

- [ ] **Step 4: Run GREEN**

Run: `cargo test -p pca-agent-core --test communication_process && cargo test -p pca-agent-core --test cloud_control_process`  
Expected: PASS; control revision changes enable/disable safely and existing system collector still works.

- [ ] **Step 5: Commit**

```bash
git add agent/core
git commit -m "feat: run paired wechat provider"
```

### Task 6: Add an isolated Agent Communication sync protocol

**Files:**
- Modify: `agent/core/src/cloud_control.rs`
- Modify: `agent/core/src/app.rs`
- Modify: `crates/db-local/src/actor.rs`
- Modify: `crates/db-local/src/repository.rs`
- Modify: `apps/cloud-api/src/sync.ts`
- Create: `agent/core/tests/communication_sync.rs`
- Modify: `apps/cloud-api/src/test/event-sync.test.ts`

**Consumes:** Task 4 pending/ack APIs and Task 1 schema.

**Produces:** `ControlClient::sync_communication_events` that acknowledges only the exact accepted-or-duplicate local IDs.

- [ ] **Step 1: Write failing acknowledgement tests**

```rust
#[tokio::test]
async fn malformed_or_partial_ack_keeps_communication_outbox_pending() {
    let client = FakeControlClient::reply_with_duplicate_and_accepted_same_id();
    assert!(sync_pending_communication_events(&store, &client, credentials).await.is_err());
    assert_eq!(store.pending_communication_count().await.unwrap(), 1);
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test -p pca-agent-core --test communication_sync`  
Expected: FAIL because the communication sync method does not exist.

- [ ] **Step 3: Implement strict separate endpoint use**

Send at most 200 valid message events in FIFO order to
`/v1/agent/sync/communication/events`. Build a fresh HTTP client for each operation so VPN/
proxy changes do not require Agent restart. Validate returned IDs are an exact disjoint partition
of requested IDs; reject partial, unknown, duplicate-across-buckets, or rejected responses without
acknowledging local rows. Do not modify the existing system-event allowlist.

- [ ] **Step 4: Run GREEN**

Run: `cargo test -p pca-agent-core --test communication_sync && pnpm --filter @pca/cloud-api test -- event-sync`  
Expected: PASS; system endpoint continues rejecting high-sensitivity communication events.

- [ ] **Step 5: Commit**

```bash
git add agent/core crates/db-local apps/cloud-api/src/sync.ts apps/cloud-api/src/test/event-sync.test.ts
git commit -m "feat: sync communication events separately"
```

### Task 7: Persist Cloud communication projections and enforce Owner scope

**Files:**
- Create: `packages/db-cloud/migrations/0006_wechat_communication.sql`
- Modify: `packages/db-cloud/src/schema.ts`
- Modify: `packages/db-cloud/src/repository.ts`
- Modify: `packages/db-cloud/tests/repository.test.ts`
- Modify: `apps/cloud-api/src/index.ts`
- Create: `apps/cloud-api/src/test/communication.test.ts`
- Modify: `apps/cloud-api/src/migrate.ts`
- Modify: `scripts/verify_migrations.py`
- Modify: `scripts/verify_cloud_migrations.py`

**Consumes:** Task 6 request shape and device authentication.

**Produces:** idempotent message ingestion plus Owner-only device conversation/message query methods.

- [ ] **Step 1: Write failing Cloud tests**

```ts
test("communication message is idempotent and invisible across workspaces", async () => {
  await syncCommunication(deviceA, validCommunicationBatch());
  await syncCommunication(deviceA, validCommunicationBatch());
  assert.equal((await listMessages(ownerA, deviceA)).length, 1);
  assert.equal(await responseStatus(ownerB, `/v1/devices/${deviceA}/communication`), 404);
});
```

- [ ] **Step 2: Run RED**

Run: `pnpm --filter @pca/cloud-api test -- communication`  
Expected: FAIL because the migration, repository and routes do not exist.

- [ ] **Step 3: Implement immutable, indexed projections**

Create tables for conversations, messages, message attachments, objects and tombstones. Enforce
device/workspace composite foreign keys, source-key uniqueness, permitted direction/type values,
attachment manifest hashes and no body/blob in object records. Add endpoint-level schema validation,
device credential auth for ingest, Owner session auth for reads, and `limit` bounds. Store only R2
object keys that are opaque UUID paths, never display strings.

- [ ] **Step 4: Run GREEN plus migration replay**

Run: `pnpm --filter @pca/db-cloud test && pnpm --filter @pca/cloud-api test -- communication && python3 scripts/verify_cloud_migrations.py`  
Expected: PASS for empty and prior-version migration paths, idempotency, and tenant rejection.

- [ ] **Step 5: Commit**

```bash
git add packages/db-cloud apps/cloud-api scripts
git commit -m "feat: store private communication projections"
```

### Task 8: Add private R2 upload, completion and signed read access

**Files:**
- Modify: `apps/cloud-api/package.json`
- Modify: workspace lockfile
- Create: `apps/cloud-api/src/r2.ts`
- Modify: `apps/cloud-api/src/index.ts`
- Create: `apps/cloud-api/src/test/r2.test.ts`
- Modify: `apps/cloud-api/src/test/communication.test.ts`
- Modify: `docs/runbooks/S1B_PAIRING_REPAIR.md`

**Consumes:** Task 7 object records and Attachment manifests.

**Produces:** a narrow `R2ObjectStore` port with prepare, complete and owner-read signing methods.

- [ ] **Step 1: Write failing fake-store tests**

```ts
test("only a completed attachment gets a short owner read URL", async () => {
  const object = await prepareObject(deviceA, validAttachment());
  assert.equal(await signedRead(ownerA, object.id), null);
  await completeObject(deviceA, object.id, validHead());
  assert.match((await signedRead(ownerA, object.id))!.url, /^https:/);
});
```

- [ ] **Step 2: Run RED**

Run: `pnpm --filter @pca/cloud-api test -- r2`  
Expected: FAIL because object preparation and signing do not exist.

- [ ] **Step 3: Implement the adapter behind a port**

Add `@aws-sdk/client-s3` and `@aws-sdk/s3-request-presigner` at pinned compatible versions;
both are Apache-2.0 and are required because R2 exposes an S3-compatible signed-request API.
`R2ObjectStore` creates opaque keys, signs a bounded-time `PUT`, verifies `HEAD` size/MIME/hash
metadata at completion, and signs a bounded-time `GET` only for Owner-visible completed objects.
Runtime configuration must reject missing credentials, non-HTTPS endpoint, or a public Bucket
mode. Tests use a fake port; no real secret appears in test fixtures.

- [ ] **Step 4: Run GREEN**

Run: `pnpm --filter @pca/cloud-api test -- r2 && pnpm --filter @pca/cloud-api typecheck`  
Expected: PASS; incomplete, wrong-hash, cross-workspace and non-Owner requests receive no URL.

- [ ] **Step 5: Commit**

```bash
git add apps/cloud-api package.json pnpm-lock.yaml docs/runbooks/S1B_PAIRING_REPAIR.md
git commit -m "feat: add private r2 communication objects"
```

### Task 9: Implement bounded attachment upload and durable completion

**Files:**
- Modify: `agent/core/src/cloud_control.rs`
- Modify: `agent/core/src/communication.rs`
- Modify: `crates/db-local/src/actor.rs`
- Modify: `crates/db-local/src/repository.rs`
- Create: `agent/core/tests/attachment_upload.rs`
- Create: `crates/db-local/tests/attachment_spool.rs`

**Consumes:** Tasks 4, 6 and 8.

**Produces:** resumable, hash-verified R2 media transfer with no public URL and no in-memory media queue.

- [ ] **Step 1: Write failing transfer tests**

```rust
#[tokio::test]
async fn failed_upload_keeps_private_spool_and_never_marks_attachment_complete() {
    let result = upload_attachment(&store, failing_http_client(), attachment_id()).await;
    assert!(result.is_err());
    assert!(store.spool_exists(attachment_id()).await.unwrap());
    assert!(!store.attachment_complete(attachment_id()).await.unwrap());
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test -p pca-agent-core --test attachment_upload`  
Expected: FAIL because upload state transitions do not exist.

- [ ] **Step 3: Implement prepare/upload/complete state machine**

For each accepted event, request a prepare URL, stream the spool file with a timeout and byte
limit, then call complete. Persist `prepared`, `uploading`, `completed`, and retry metadata in
SQLite; retry only retryable network failures with the existing bounded policy. Revalidate file
size and SHA-256 before upload. Delete a spool file only after Cloud completion and only when it is
not otherwise retained. A VPN/proxy switch creates a fresh request path on retry.

- [ ] **Step 4: Run GREEN**

Run: `cargo test -p pca-agent-core --test attachment_upload && cargo test -p pca-db-local --test attachment_spool`  
Expected: PASS for offline retry, switched proxy, hash mismatch, restart and exact-once completion.

- [ ] **Step 5: Commit**

```bash
git add agent/core crates/db-local
git commit -m "feat: upload communication media durably"
```

### Task 10: Add retention worker and non-revival Tombstones

**Files:**
- Create: `apps/cloud-worker/package.json`
- Create: `apps/cloud-worker/src/retention.ts`
- Create: `apps/cloud-worker/src/test/retention.test.ts`
- Modify: `apps/cloud-worker/README.md`
- Modify: `packages/db-cloud/src/repository.ts`
- Modify: `packages/db-cloud/tests/repository.test.ts`
- Modify: `crates/db-local/src/repository.rs`
- Create: `crates/db-local/tests/communication_retention.rs`

**Consumes:** Tasks 4 and 7–9.

**Produces:** idempotent daily cleanup that hides content before deletion and prevents a stale device from re-uploading it.

- [ ] **Step 1: Write failing expiry tests**

```ts
test("expiry tombstones message before deleting its private object", async () => {
  await expireDueCommunication(clockAtDay181);
  assert.equal((await listMessages(ownerA, deviceA)).length, 0);
  assert.equal(await hasTombstone(messageId), true);
  assert.equal(fakeR2.deletedKeys(), [opaqueObjectKey]);
});
```

- [ ] **Step 2: Run RED**

Run: `pnpm --dir apps/cloud-worker test -- retention`  
Expected: FAIL because the Worker and expiration service do not exist.

- [ ] **Step 3: Implement daily idempotent jobs**

Create a Worker-owned retention service with injected clock, repository and object-store port.
It selects due records in bounded pages, removes them from query eligibility, writes a Tombstone,
deletes R2 with retries, and clears Cloud message body/display fields/search copies. Local cleanup
deletes due body/spool data and applies downloaded Tombstones before attempting sync. A device
receiving a Tombstone must mark matching Outbox/message source keys terminal and must not upload.

- [ ] **Step 4: Run GREEN**

Run: `pnpm --dir apps/cloud-worker test -- retention && cargo test -p pca-db-local --test communication_retention`  
Expected: PASS for 180-day expiry, repeated job invocation, object-delete failure and offline-device non-revival.

- [ ] **Step 5: Commit**

```bash
git add apps/cloud-worker packages/db-cloud crates/db-local
git commit -m "feat: expire communication media after 180 days"
```

### Task 11: Build the minimal Owner Dashboard experience

**Files:**
- Modify: `apps/web-dashboard/src/lib/api.ts`
- Create: `apps/web-dashboard/src/lib/communication.ts`
- Modify: `apps/web-dashboard/src/app/devices/[deviceId]/page.tsx`
- Create: `apps/web-dashboard/src/app/devices/[deviceId]/communication/page.tsx`
- Modify: `apps/web-dashboard/src/app/globals.css`
- Create: `apps/web-dashboard/test/communication.test.ts`
- Modify: `apps/web-dashboard/test/device-config.test.ts`

**Consumes:** Tasks 1, 7 and 8 Owner APIs.

**Produces:** Owner-only control disclosure, provider health, conversation list, message viewer and short-lived media preview links.

- [ ] **Step 1: Write failing rendering tests**

```ts
test("communication page labels two-way small-group scope and never renders incomplete media", () => {
  const html = renderCommunicationPage(fixtureWithPendingAndCompletedAttachment());
  assert.match(html, /one-to-one and groups of at most 8/);
  assert.match(html, /180 days/);
  assert.doesNotMatch(html, /pending-object-url/);
});
```

- [ ] **Step 2: Run RED**

Run: `pnpm --filter @pca/web-dashboard test -- communication`  
Expected: FAIL because the page and rendering helpers do not exist.

- [ ] **Step 3: Implement narrow pages**

Add a Communication link to device detail, display v2 scope and Provider state/error code, and
use server-side Owner-scoped API calls for paginated conversations/messages. Render text with
escaped plain text; use native `img`, `audio`, and `video` only for completed API-signed URLs.
Do not expose object keys, secrets, unsupported message types, bulk download, search or export.

- [ ] **Step 4: Run GREEN**

Run: `pnpm --filter @pca/web-dashboard test -- communication && pnpm --filter @pca/web-dashboard typecheck`  
Expected: PASS for waiting/unsupported/disabled empty states and completed media only.

- [ ] **Step 5: Commit**

```bash
git add apps/web-dashboard
git commit -m "feat: view private communication media"
```

### Task 12: Run end-to-end gates and perform Owner-controlled rollout

**Files:**
- Modify: `docs/runbooks/S1B_PAIRING_REPAIR.md`
- Create: `docs/runbooks/WECHAT_MULTIMEDIA_ACCEPTANCE.md`
- Modify: `README.md`

**Consumes:** Tasks 1–11 and an Owner-provisioned private R2 Bucket.

**Produces:** reproducible test evidence, a safe deployment checklist, and a live acceptance procedure.

- [ ] **Step 1: Write failing acceptance fixture assertions**

```bash
pnpm --filter @pca/cloud-api test -- communication r2
cargo test -p pca-agent-core --test communication_process --test communication_sync --test attachment_upload
```

Expected before the preceding tasks are complete: at least one command fails because the implementation is absent.

- [ ] **Step 2: Add the runbook commands and evidence checklist**

Document the exact checks: private Bucket policy, R2 credential presence without printing values,
migration replay, R2 fake-port tests, device pairing, config audit, one direct and one `<= 8` group
test for each direction/type, Dashboard preview, 180-day simulated cleanup, and disabled/no-probe
proof. State that a failed WeChat compatibility probe is a valid fail-closed result, not a reason
to substitute UI/keyboard capture.

- [ ] **Step 3: Run all automated gates**

Run: `./scripts/verify-full.sh`  
Expected: PASS. Also run the targeted Rust, TypeScript, migration and R2 fake-port commands from
Tasks 1–11. Record every command, exit code and any skipped environment-only test in the runbook.

- [ ] **Step 4: Run Owner-controlled live acceptance after explicit deployment approval**

Provision the R2 Bucket and credentials outside the repository; do not alter existing Railway
variables without a new explicit instruction. Deploy migration/API/Dashboard/Worker only after
the Owner approves the exact target configuration. Verify an eligible direct conversation and an
eligible `<= 8` group for text/audio/image/video in both directions, then confirm a larger group
and unsupported Schema create no Event, no object and no Dashboard item.

- [ ] **Step 5: Commit docs and hand off rollout evidence**

```bash
git add docs README.md tasks/DEFINITION_OF_DONE.md
git commit -m "docs: add wechat multimedia acceptance runbook"
```

## Plan Self-Review

- **Scope coverage:** Tasks 1, 2, 3 and 5 enforce exact authorization, source proof and non-
  interaction; Tasks 4, 6 and 9 establish atomic local durability and isolated sync; Tasks 7 and
  8 establish tenant isolation, private R2 and signed access; Task 10 implements 180-day deletion
  and non-revival; Task 11 provides the minimum Dashboard; Task 12 covers the live gates.
- **No placeholders:** no unowned dependency, unspecified endpoint, broad media type, retention
  duration, group threshold, or public-storage fallback remains.
- **Interface consistency:** all subsequent tasks consume the v2 scope and
  `CommunicationMessageRecorded`/`CommunicationAttachment` from Task 1; all media flows use the
  Task 8 `R2ObjectStore` port; no Provider task imports the Agent Cloud client.

# Architecture

## 1. Final topology

```text
Web Dashboard (Next.js)
        |
Cloud API (Hono / Better Auth)
        |
PostgreSQL + R2 + Background Jobs
        |
HTTPS / versioned REST
        |
Rust agentd (LaunchAgent, resident)
├── Runtime Supervisor
├── Collector Registry
├── Event Bus
├── Projection
├── SQLite DbActor
├── Local Outbox / Sync Engine
├── Command Worker
├── Health / Update Handshake
├── Communication Provider Supervisor
└── Native Messaging Host coordination
        |
0600 Unix Domain Socket
length-prefixed MessagePack/JSON
        |
Swift PlatformBridge
├── ScreenCaptureKit
├── Accessibility / AXObserver
├── NSWorkspace
├── Core Location
├── FSEvents
├── Power / Sleep-Wake
├── TCC probes
└── SMAppService / Setup / Repair / Sparkle coordination
```

## 2. Process boundaries

### 2.1 S1A installation channel

The S1A self-use channel installs only to
`$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app`.
Its persistent `Data/` and ephemeral `Run/` directories are siblings under the
same root; bundle replacement changes only `App/`.

`agentd` runs through a user-level `SMAppService` LaunchAgent in the logged-in
user session. S1A never uses root, a LaunchDaemon, or a privileged helper. The
`/Applications/PersonalComputerAgent.app` location in the product specification
is reserved for a separately approved future public channel, not an S1A
fallback or simultaneous target. See `docs/INSTALLATION_CHANNELS.md` and
`docs/adr/ADR-0005-user-level-self-install-channel.md`.

The topology's Sparkle coordination is a later release capability and is not
part of S1A.

### Rust `agentd`

负责：

- Agent 主状态机
- Collector/Provider 生命周期
- Event 生成后的接收、序列化与持久化
- SQLite 单写入 DbActor
- Projection、Outbox、Sync、Command、Heartbeat
- Retention/Tombstone 本地执行
- Provider Cursor 和 crash recovery

不负责：

- 直接调用 Apple UI/System Framework
- 弹出权限 UI
- 修改第三方 App
- 日常产品界面

### Swift PlatformBridge

负责：

- Apple Framework 调用
- 系统权限和 capability probe
- Sleep/Wake
- Setup/Repair
- SMAppService
- Sparkle Update Coordinator

不负责：

- 业务状态机
- SQLite
- Cloud
- Sync
- Retention
- Communication Provider

### Web Dashboard

负责：

- 查询、筛选、展示、配置
- 设备状态
- Command 创建
- 导出和删除流程

不负责：

- 直接访问数据库
- 直接连接本地 Agent
- 推断本地权限已经生效

## 3. Local IPC

固定合同字段：

```text
protocol_version
request_id
capability
deadline
payload
error_code
```

安全基线：

- Application Support 目录 0700
- Socket 0600
- 双向 nonce/shared-secret 握手
- 未知协议版本拒绝
- 请求必须有 deadline
- Bridge crash 只降级依赖能力，不终止 Rust Core

## 4. Event pipeline

```text
Platform/Provider Source
        ↓
Collector / CommunicationProvider
        ↓
EventEnvelope
        ↓
Event Bus
        ↓
SQLite transaction:
  Event Store
  Projection mutation
  Sync Outbox
        ↓
Sync Worker
        ↓
Cloud Batch API
        ↓
PostgreSQL + Object Storage
        ↓
Dashboard query projections
```

不变量：

- Collector 不调用 Cloud。
- Event 与 Outbox 在同一事务提交。
- Event ID 由设备生成并全局幂等。
- Attachment 先创建本地引用，再走预签名上传和 complete。
- ACK 只有在服务端业务事务提交后返回。
- 重试不产生重复副作用。

## 5. WeChat Provider

正常生命周期：

```text
disabled
  → waiting_source
  → checking_stored_key
  → passive_scanning
  → verifying_database
  → active
```

规则：

- WeChat 未运行/未登录：静默等待。
- 有 Keychain KeyMaterial：先验证，不重复提取。
- 无 Key 或失效：仅被动扫描当前已登录进程。
- 正常流程不得 kill/open/re-sign WeChat，不提示登录。
- Passive scan 失败：退避并保持 capability_unavailable/unsupported。
- LLDB Active Extraction 仅显式 Repair/Developer Mode，V0 正常路径不自动执行。
- 获取成功后直接 SQLCipher 只读打开；不在公共 `/tmp` 落完整明文数据库。
- `session.db`/WAL 变化只用于定位更新会话；真实消息通过 per-talker `sort_seq` Cursor 拉取。
- Cursor 更新必须在消息持久化成功后发生。

## 6. Repository dependency direction

```text
Web UI → Web Application → Domain TS → API Client
Cloud API → Application → Domain → Ports → Infrastructure
Rust Collector → Domain Event Contract → EventSink
Swift Bridge → Bridge Protocol only
Communication Provider → Provider Contract → EventSink
```

禁止反向依赖和跨包深层导入。

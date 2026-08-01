# WeChat 双向多媒体采集设计

**日期：** 2026-08-02  
**状态：** 待用户审核  
**替代：** `2026-07-31-wechat-outbound-message-collector-design.md` 的实施范围；旧文档保留为历史决策记录。  
**前置条件：** 已完成 S1B 真实配对、设备鉴权、控制面配置、Local Event/Outbox 和系统指标同步。

## 1. 目标与范围

为私有 Owner Workspace 的已配对 macOS 设备提供一个 `communication.wechat`
Provider。它只读 WeChat 的本地消息数据库、WAL 与已落地媒体文件，在满足
严格来源证据时，将允许的消息同步到私有 Cloud，并在 Dashboard 查看。

本次确认的精确范围：

- 收到和发出的消息；
- 文本、语音、图片、视频；
- 一对一会话，以及 Provider 能可靠确认总成员数 **小于或等于 8** 的群；
- 本地、Cloud 索引和 R2 原件保存 180 天；
- 原件放入私有 Cloudflare R2 Bucket，Dashboard 使用短期签名 URL 预览或播放。

以下内容不在范围内：键盘钩子、IME、草稿、剪贴板、通知内容、屏幕/Accessibility
抓取；10 人及以上群、成员数不明的群；联系人档案与群成员名单；表情、转账、位置、
小程序、文件；向 WeChat 写入、发送消息、启动/终止/重签/注入/登录 WeChat；
自动 Active Extraction；Telegram 与其他应用。

“消息撤回”不触发 PCA 自动删除；用户的 PCA 删除请求和 180 天保留期才会删除。

## 2. 设计选择

采用项目内 Rust Provider，并参考 `pandorafuture/wx-cli` 的 Keychain、SQLCipher、
WAL 与 per-talker Cursor 思路；**不**执行或依赖其 CLI。Provider 是唯一了解
WeChat 私有数据格式的组件。微信版本、密钥或 Schema 无法证明时，它安全停止，不以
UI 变化、未读数、输入事件或猜测补足证据。

R2 Bucket 必须保持私有，禁止 public URL。Cloud API 仅在 Owner 已认证、Workspace 与
Device 均匹配、对象属于可见消息时签发短效 `GET` URL。媒体上传使用短效受限 `PUT`
URL，Cloud API 在完成时验证对象存在、长度、MIME 与 SHA-256；未完成对象永不展示。
TLS 与 R2 加密静态存储为最低要求；本阶段不引入另一套客户端加密/密钥分发体系。

## 3. 授权与配置门禁

现有只支持 `outgoing + text + full` 的 `communication.wechat` 配置，必须替换为
严格且不可任意扩张的 v2 配置合同：

```json
{
  "enabled": true,
  "directions": ["incoming", "outgoing"],
  "message_types": ["text", "audio", "image", "video"],
  "conversation_scope": "direct_and_group_at_most_eight_members",
  "max_group_members": 8,
  "sync_mode": "full",
  "retention_days": 180
}
```

该完整形状是唯一允许值；缺字段、未知字段、不同上限或放宽会被 Dashboard、Cloud API
和 Agent 拒绝。Dashboard 开关需展示“收发文本、语音、图片、视频；一对一及不超过
8 人的群；保存 180 天”，Owner 的每次启停均产生含旧/新配置、设备、操作者、时间与
修订号的审计记录。

Provider 只有在有效配对、同 Workspace 的完整 v2 配置已启用且配置修订不落后时才可
探测任何 WeChat 来源。禁用后立刻停止新读取；禁用前已原子落盘的 Outbox 项仍按其
创建时的已授权修订完成同步，但不会再产生新项。禁用不自动删除历史；删除另走明确的
PCA 删除流程。

## 4. 本地读取与资格校验

```text
WeChat 数据库 / WAL / 已落地媒体（只读）
  -> WechatProvider（版本、密钥、Schema、会话大小探测）
  -> 每条消息的资格门
  -> DbActor 原子提交 Event + 投影 + Attachment spool + Outbox + Cursor
  -> Agent 同步与 R2 上传
  -> Cloud 投影与 Dashboard
```

Provider 的状态使用已有 `ProviderStatus`：无配置或未配对为 `disabled`；WeChat 未运行
或未登录为 `waiting_source`；Keychain 校验为 `checking_stored_key`；被动扫描为
`passive_scanning`；只读 DB 校验为 `verifying_database`；正常为 `active`；不可读取、
不支持版本或证据不足分别进入统一错误状态。正常路径绝不弹窗、要求登录、申请额外系统
权限或修改 WeChat。

一条消息仅当 Provider 从已支持的数据源同时证明以下事实才会进入本地库：

1. 属于当前本机 WeChat 账号；
2. 方向是 `incoming` 或 `outgoing`；
3. 类型恰为允许的四种之一；
4. 消息是最终、持久化的记录；发出消息还必须有成功发送状态；
5. 会话是一对一，或可靠的群总成员数 `<= 8`；
6. 文本正文非空，或媒体原件已完整落地并能计算 MIME、字节数和 SHA-256。

无法证明其中任意一点即跳过记录并保留 Cursor，不保存正文、媒体、群成员信息或推测结果。
为判断群规模可读取源元数据中的计数，但 PCA 不持久化群成员名单。媒体尚未下载完成时
也不生成消息：Cursor 保持在上一个已提交位置，稍后从原记录重试。

KeyMaterial 仅存在 macOS Keychain；SQLite 仅保存 credential reference、版本、验证
时间与无秘密状态。诊断包和普通日志不得包含正文、会话名、文件路径、缩略图或密钥。

## 5. 本地持久化、去重与背压

每个源消息以 `account_id + conversation_id + server_id/sort_seq` 导出稳定 source key，
并产生 `communication.message_recorded` schema v1 Event，`source=communication.wechat`
和 `sensitivity=high`。payload 只包含消息、会话和附件 manifest，不含 KeyMaterial。

`DbActor` 在一个 SQLite 事务中写入：不可变 Event、Conversation/Message 投影、每个
附件的私有 spool 引用和 hash manifest、同步 Outbox，以及每账号/会话 Cursor。source key
和 attachment hash 约束保证重复 WAL 通知、重启、崩溃恢复和重试最多产生一次消息投影。

为保证原件在断网时不被 WeChat 清理，Agent 在同一落盘成功路径把完整媒体拷贝到
PCA 私有 attachment spool；spool 目录权限仅允许当前用户，受配额和 Outbox 高/低水位
共同限制。若没有空间、无法复制或 Outbox 已到高水位，则不推进该 Cursor、不在内存保留
正文/媒体，稍后重新读取。Cloud 确认对象完成且本地保留策略允许后，才删除临时上传副本。

## 6. 云端同步、R2 与查询

由于现有 `/v1/agent/sync/events` 仅接受非敏感系统事件，本阶段新增独立的受设备凭据
保护的 Communication 同步面，禁止把高敏感消息塞进该通用接口：

1. `POST /v1/agent/sync/communication/events`：严格验证 v1 消息合同、设备/Workspace、
   当前或创建时的授权修订与稳定幂等键，原子写入 Cloud message/conversation 投影；
2. `POST /v1/agent/objects/prepare`：只对已接受消息的 attachment manifest 签发受限 R2
   上传 URL；
3. Agent 上传到私有 R2 后调用完成端点；Cloud 校验对象 hash/size/MIME 并标记可见；
4. Owner Dashboard 通过 Workspace/Device 作用域读取会话、消息和 Provider 健康；媒体预览
   端点只为已完成、未过期对象签发短效下载 URL。

Cloud 表拆分为 conversations、messages、message_attachments、object records、配置审计
与 retain/delete tombstones。Postgres 永不保存二进制原件；R2 key 不可由前端猜出且不含
会话名或消息正文。所有查询必须 Owner 身份、Workspace 和 Device 交集过滤。

## 7. 180 天保留与删除

每日保留任务按 `occurred_at` 计算到期时间，先让内容退出 Dashboard/API 查询和签名资格，
再创建 Tombstone，物理删除 R2 原件、Cloud 正文/显示名/搜索副本和本地正文/媒体副本。
删除操作可重试、幂等，并分别记录对象删除失败而不重新暴露内容。到期后可保留不含内容的
审计与删除事实。用户显式删除沿同一 Tombstone 路径执行；离线设备收到 Tombstone 后不得
重新上传旧 Outbox 或复活内容。

## 8. Dashboard 最小界面

设备详情保留现有开关与审计卡，增加 Communication 入口：

- Provider 状态、最后成功、错误码、配置修订、Outbox/媒体待传数量；
- 会话列表：会话名、direct/group、最后允许消息时间、消息数；
- 消息页：方向、时间、文本和已完成附件；图片预览、音频/视频原生播放；
- 空状态、禁用状态、等待来源和不支持状态清晰区分；
- 保留期说明和“180 天后自动删除”提示。

不做全文搜索、导出、批量下载、跨设备聚合或复杂视觉设计。

## 9. 失败与安全行为

- 不支持的 WeChat 版本、Schema、密钥或群规模字段：fail closed，不采集、不上传；
- VPN/代理切换：请求失败后按既有有界重试与新连接策略恢复，不要求重启 Agent；
- 网络/R2 故障：本地已落盘的 Outbox/spool 等待重试，不丢失、不重复；
- 文件过大、MIME 与扩展名不一致、hash 不匹配、媒体未完整落地：拒绝该附件，并不推进
  依赖它的消息 Cursor；
- R2 凭据缺失或 Bucket 非私有：Cloud API 启动前配置检查失败，媒体同步不得降级到公开 URL
  或 Postgres blob；
- 任何未知配置字段、未知 Event 字段或权限/租户不匹配：显式拒绝并保留本地内容等待诊断。

## 10. 预期改动边界

- 新增 `crates/wechat-provider/`，仅封装 WeChat 的只读 source adapter 与 fixture contracts；
- `crates/provider-contracts/`、`agent/core/`、`crates/db-local/`：Provider Supervisor、v2
  配置、Event/Projection/Cursor/Attachment spool/Outbox 原子事务；
- `contracts/`、`packages/contracts/`：v2 控制面和 Communication Event/Attachment 合同；
- `packages/db-cloud/`、`apps/cloud-api/`、`apps/cloud-worker/`：迁移、私有 R2 适配器、
  同步、签名访问、Tombstone 与保留任务；
- `apps/web-dashboard/`：配置说明、Provider 状态、会话/消息/媒体查看；
- 数据字典、ADR、运行手册与测试 fixtures。

不会改动 Railway 公网域名、既有数据库变量或密钥。R2 Bucket、R2 API 凭据、私有访问
策略和 Cloud API 的新增 R2 配置由 Owner 在后续部署阶段提供；本设计不授权修改这些配置。

## 11. 验收与验证

1. 合同 fixtures 覆盖双向文本、语音、图片、视频、一对一、小群；并覆盖大群、人数未知、
   草稿、失败发送、空消息、未知类型、未完成媒体和未知字段的拒绝路径。
2. 本地事务与崩溃测试证明同一 source key 至多一个 Event、投影、Outbox 与 Cursor 推进；
   WAL 重复、重启、睡眠唤醒和断网补抓均不重复或丢失。
3. 未配对、配置禁用、WeChat 未登录、Key/DB/Schema 不兼容时不读数据、不启动或修改 WeChat，
   且其余 Agent 与系统指标同步继续工作。
4. 云端集成测试证明设备/Workspace 隔离、Owner 认证、重复请求幂等、未完成对象不可见、
   非 Owner 和跨 Workspace URL 请求被拒绝。
5. R2 集成冒烟测试证明私有 Bucket、上传 hash 校验、短效访问 URL、过期后不可访问；不在
   Cloud API、Dashboard 或日志泄露 R2 secret。
6. 180 天的 Local、Cloud、R2 到期/显式删除/Tombstone/离线重连测试证明内容不复活。
7. Dashboard E2E 覆盖启停配置、状态、会话列表、双向文本与各媒体类型查看/播放、等待和
   不支持空状态。
8. 仅当真实已支持的 WeChat 版本通过版本、密钥和 Schema Probe 后，执行一次手工验收：
   一对一与 `<= 8` 人群的收发四种类型各一条，Dashboard 可见且附件可预览；不兼容版本
   以安全状态为验收结果，不能以任何抓键盘或 UI 抓取替代。

## 12. 实施分段

1. 合同、v2 授权配置、Provider 状态和只读 source fixtures；
2. Local Event/Projection/Cursor/attachment spool 原子链路与失败恢复；
3. Cloud 通信投影、私有 R2 上传/签名访问与 180 天 Tombstone 保留任务；
4. Dashboard 会话、消息、媒体与状态界面；
5. R2 已由 Owner 配置后进行真实设备验收、安装包构建与部署。

每段先写失败路径测试，再实现最小纵向代码；未经下一段的验证，不扩展类型、会话范围或
存储权限。

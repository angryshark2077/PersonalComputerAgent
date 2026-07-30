# Personal Computer Agent Engineering Rules

## 1. Facts and scope

- 开始任何架构、Schema、Provider、Collector、Bridge、Sync 或安全改动前，必须阅读 `docs/PRODUCT_TECH_SPEC_V1.1.md` 对应章节。
- 必须同时阅读 `docs/SOURCE_ERRATA.md`。
- V0 是 Web-dashboard-first、Rust Core、Swift macOS Bridge、Event-driven、Privacy-by-design。
- V0 不包含 Electron 本地工作台、AI、Keylogger、摄像头、麦克风、微信消息发送、远程桌面或 Computer Use。
- 产品只面向设备所有者或明确授权使用者；静默运行仅发生在产品级授权之后。

## 2. Architecture invariants

- Rust `agentd` 是本地业务状态和运行时事实源。
- Swift PlatformBridge 只封装 Apple API、TCC、Power、FSEvents、SMAppService 和 Setup/Repair。
- Swift 不访问业务 SQLite、Cloud API、Retention、Sync、Command 或 Communication Provider 状态。
- Collector 只能产生标准 Event 和 Attachment 引用。
- Collector 不得直接访问 Cloud API，不得自行创建云端业务表。
- Event 先进入 Local Event Store/Projection/Outbox，再由 Sync Engine 上传。
- Web UI 不得导入 Drizzle、数据库实现、Provider SDK 或本地系统能力。
- Cloud Domain 只依赖 Port；第三方 SDK 只能位于 Infrastructure Adapter。
- Rust Core 不依赖 wx-cli CLI 文本/JSON 输出；WeChat 只通过 `CommunicationProvider` Port 和内部 Adapter crate。
- 正常 WeChat 路径不得 kill/open/re-sign WeChat，不得提示登录，不得自动进入 LLDB Active Extraction。
- Provider 只能返回状态和错误；不得直接触发弹窗、启动第三方 App 或修改第三方 App。
- 本地 Bridge 使用版本化合同；任何 breaking change 必须提升 `protocol_version` 并保留兼容窗口。

## 3. Consent, privacy and secrets

- 任何敏感 Collector 在启用前必须有产品级授权、采集范围和保留策略。
- “后台静默”不等于绕过 TCC、SIP、系统权限或用户授权。
- WeChat KeyMaterial、device token、Bridge shared secret、OAuth token 只进入 Keychain/Server Secret Store。
- SQLite 只保存 `credential_ref`、版本、验证时间和非秘密状态。
- Event Payload、日志、诊断包默认不得包含 Secret。
- 不读取 Cookie、密码、表单、连续剪贴板或键盘输入。
- 删除必须使用 Tombstone，离线设备不得复活已删除数据。

## 4. Database and contracts

- Event 事实 append-only；不得原地修改历史 Event。
- 所有 Schema 改动必须同时包含：
  - Migration
  - 数据字典
  - 索引评估
  - Cloud/Local/Sync 影响
  - Backfill/前滚方案
  - 测试
- 已发布 Migration 永久冻结；错误通过新 Migration 修复。
- JSON Schema、Rust DTO、Swift DTO、OpenAPI 和数据库枚举不得独立漂移。
- Unknown fields 或 unknown enum 不得被安全关键路径静默忽略。
- 幂等键必须由调用方稳定生成；重试不得产生重复 Event、Command 或对象上传。

## 5. Implementation discipline

- 编码前说明范围、假设、成功标准和修改文件。
- 先检查现有实现，再新增抽象。
- 实现满足合同的最小完整纵向切片。
- 只修改当前任务需要的代码；禁止顺手重构相邻模块。
- 不新增依赖，除非说明现有工具为什么不足、许可证和供应链影响。
- 不使用无界队列、无界重试、无超时子进程或不可取消长任务。
- `unsafe` 必须有局部安全说明、边界测试和替代方案说明。
- Provider/Collector 错误必须映射到统一 Error Code。
- 任何未解决 TODO 必须关联 Issue/Sprint，不得以 TODO 代替本任务必须实现的内容。

## 6. Required verification

每次完成前按适用范围真实执行：

- format
- lint
- Rust build / clippy `-D warnings`
- Swift build / strict concurrency checks
- TypeScript typecheck
- unit tests
- contract tests
- migration replay：空库与上一支持版本
- dependency boundary checks
- failure-path tests
- package/smoke test
- security/privacy impact review
- documentation/ADR update

禁止声称未执行的测试已通过。报告必须包含命令、退出码和关键结果。

## 7. Completion rule

只有满足 `tasks/DEFINITION_OF_DONE.md` 以及当前 Sprint 的退出门禁，才能声明完成。  
“代码已写完”“本地看起来正常”“理论上可行”不构成完成证据。

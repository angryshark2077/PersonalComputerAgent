**PERSONAL COMPUTER AGENT**

**产品与技术规格**

Web-dashboard-first · Rust-core · Swift-macOS-bridge · Event-driven · Privacy-by-design

V1.1 · Rust Core 与静默 WeChat Provider 重构版 · 2026-07-30

> 本版在 V1.0 基础上完成 Agent Runtime 与 WeChat Provider 的架构重构。Personal Computer Agent 改为 Rust Core Runtime 主导：Collector 调度、Event Bus、SQLite、Outbox、同步、命令、健康检查和 Provider 生命周期均由 Rust 实现；Swift 仅作为 macOS Platform Bridge，负责 ScreenCaptureKit、Accessibility、NSWorkspace、Core Location、FSEvents、Power/TCC 与 ServiceManagement。WeChat 不再通过反复执行 CLI 或要求用户在登录界面操作：在用户已完成产品级授权的前提下，Agent 后台静默等待 WeChat 进程与数据库就绪，优先复用 Keychain 中已验证的 KeyMaterial；缺失或失效时，仅对当前已登录、正在运行的 WeChat 执行被动内存扫描，不主动退出、启动、重签名或提示登录。获取成功后直接以 SQLCipher 只读方式打开数据库，使用 session/WAL 变化检测、逐会话 sort_seq Cursor 和 SSE 等价的内部事件流完成近实时同步。V0 默认不自动执行会重启 WeChat 的 LLDB Active Extraction；该能力只保留为开发者或显式维修模式。

GUIDE 文档导航

本文件是 Personal Computer Agent 当前产品与技术决策的主规格事实源。正文保存最终决策、产品语义、状态机、运行时合同和实现边界；字段级字典、枚举、错误码、工程模板和检查单统一放在附录。

<table>
<colgroup>
<col style="width: 50%" />
<col style="width: 50%" />
</colgroup>
<thead>
<tr class="header">
<th><strong>部分</strong></th>
<th><strong>章节</strong></th>
</tr>
</thead>
<tbody>
<tr class="odd">
<td>第一部分：产品定义与全局规则</td>
<td>1. 执行摘要与最终决策<br />
2. 产品定位、目标用户与边界<br />
3. 数据分类、隐私、授权与保留<br />
4. 核心领域对象与生命周期</td>
</tr>
<tr class="even">
<td>第二部分：产品信息架构与用户流程</td>
<td>5. 一级信息架构与 Web 工作台<br />
6. 首次安装、设备配对与权限流程<br />
7. Overview 工作台<br />
8. Timeline 与 Screenshot Explorer<br />
9. Activity、Browser 与工作时长<br />
10. Communication、Files、Location、Devices 与 Settings</td>
</tr>
<tr class="odd">
<td>第三部分：Agent、Collector 与同步运行时</td>
<td>11. 总体系统与部署架构<br />
12. macOS Agent 与进程设计<br />
13. Collector Framework<br />
14. Screenshot、Activity 与 System Collector<br />
15. Browser、File 与 Location Collector<br />
16. Communication Adapter 与 WeChat CLI<br />
17. Sync Engine、远程命令与自动更新</td>
</tr>
<tr class="even">
<td>第四部分：数据、API 与安全架构</td>
<td>18. 领域数据、标识与事实源边界<br />
19. Cloud PostgreSQL Schema<br />
20. Local SQLite Schema<br />
21. Sync Contract、冲突、删除与保留<br />
22. API、Object Storage 与 Adapter 合同</td>
</tr>
<tr class="odd">
<td>第五部分：设计系统与工程治理</td>
<td>23. Web Dashboard UI Design System<br />
24. Monorepo、Code Agent 与工程治理<br />
25. 测试、可观测性、性能与安全门禁<br />
26. 实施路线、风险与 Sprint<br />
27. V0 产品与技术验收</td>
</tr>
<tr class="even">
<td>第六部分：附录</td>
<td>A. 术语表<br />
B. 最终决策清单<br />
C. 核心枚举与状态字典<br />
D. 核心错误码<br />
E. 数据字典<br />
F. Collector / Adapter 合同<br />
G. AGENTS.md 模板<br />
H. CLAUDE.md 模板<br />
I. Migration / PR 检查单<br />
J. UI 页面、抽屉与弹窗清单<br />
K. ADR 索引与技术参考</td>
</tr>
</tbody>
</table>

**PART I 第一部分：产品定义与全局规则**

先定义产品是什么、服务谁、收集什么，以及所有模块共同遵守的隐私、权限和领域语义。

# 1. 执行摘要与最终决策

本章是全文最高优先级的决策摘要；后续章节负责解释与实现，不得与本章冲突。

## 1.1 最终产品形态

- 唯一主界面：Web Dashboard。用户日常查看、检索、筛选、导出和管理全部在浏览器完成。

- macOS 端形态：签名并公证的 Headless Agent App Bundle。常态无 Dock 图标、无常驻窗口；只有首次安装、权限修复、诊断和更新失败时显示最小 Setup/Repair UI。

- Agent 运行方式：用户登录后由 SMAppService 注册的 LaunchAgent 自动启动；崩溃后由 launchd 与 Agent Supervisor 恢复。

- 数据采集：截图、应用活动、浏览器、通信、文件元数据、位置与系统状态全部通过独立 Collector 采集。

- 统一事实模型：所有 Collector 先生成不可变 Event；业务投影、同步和 Dashboard 不直接依赖具体 Collector 的原始实现。

- 同步方式：Local Persistent Outbox + Real-time Cloud Sync。实时上传属于同步策略，本地 SQLite 的作用是可靠队列、离线恢复和幂等，不是替代云端。

- 云端能力：身份、设备、事件存储、对象存储、查询、统计、远程命令、更新策略和 Web Dashboard。

- V0 不包含 AI 层、键盘记录、摄像头、麦克风、远程桌面或自动控制电脑。RustDesk 只保留为 V2 Action Layer 的可选集成。

## 1.2 最终产品架构原则

| **原则**                           | **最终规则**                                                                                                                                 |
|------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------|
| Web Dashboard 为主产品             | 本地 Agent 不承担日常界面；所有业务查看、检索、配置、命令和设备管理统一进入 Web Dashboard。                                                  |
| Rust Core 为运行时事实源           | Collector 生命周期、Event、SQLite、Outbox、同步、命令、健康和 Provider 由 Rust Core 统一管理；Swift 不复制业务状态机。                       |
| Swift 只承载 macOS Platform Bridge | ScreenCaptureKit、Accessibility、NSWorkspace、Core Location、FSEvents、Power/TCC 与 ServiceManagement 通过稳定 Bridge Contract 暴露给 Rust。 |
| 授权后静默运行                     | 用户在安装/设置中一次性启用某类采集后，Agent 不反复弹窗、不要求打开状态栏；数据源未就绪时后台等待，条件满足后自动初始化。                    |
| 事件驱动                           | Collector 只能产生 Event；业务投影、同步、Dashboard 与未来 Memory 层统一消费 Event。                                                         |
| 本地持久队列                       | 采集与网络解耦；断网、睡眠、进程重启和云端不可用都不得丢失已提交 Event。                                                                     |
| 云端跨设备事实源                   | PostgreSQL 负责跨设备查询、Dashboard 与账户级状态；本地保存采集事实、队列、短期投影和 Provider Cursor。                                      |
| Provider 可替换                    | WeChat、浏览器、对象存储和地图均通过 Port/Adapter 接入；Domain 不导入 Provider SDK 或 CLI 输出格式。                                         |
| 隐私最小化                         | 敏感 Collector 必须有产品级授权、范围和保留策略；凭据只进 Keychain/Secret Store，不进 Event、SQLite Payload 或日志。                         |
| 可观测与可恢复                     | 所有长任务有状态、日志、超时、取消、重试和错误分类；内部等待状态默认静默，诊断仅在用户主动打开时呈现。                                       |

## 1.3 最终技术裁决

| **领域**              | **最终选择**                                                                | **原因**                                                                                           |
|-----------------------|-----------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| Agent Core Runtime    | Rust stable + Tokio + Serde + tracing                                       | 长驻后台、并发 Collector、Provider、同步和恢复均需要可预测资源占用、强类型状态机和跨平台扩展能力。 |
| macOS Platform Bridge | Swift 6.x + Foundation/AppKit + Swift Concurrency                           | 仅封装 Apple API 和 TCC；通过 Bridge Protocol 向 Rust 提供事件与命令，不保存领域状态。             |
| 进程通信              | Unix Domain Socket + length-prefixed MessagePack/JSON；协议版本化           | 避免把 Rust/Swift 绑定在不透明 FFI 上；支持独立崩溃恢复、Fixture Contract Test 和未来替换实现。    |
| 后台注册              | SMAppService LaunchAgent 注册 Rust agentd；Swift Setup App 负责安装/修复    | Rust Core 是常驻进程；Swift App 只处理 Bundle 身份、权限入口、更新和注册。                         |
| Agent Bundle          | 签名公证的 LSUIElement App + 内嵌 Rust agentd + Swift bridge helper         | 统一 Bundle ID、签名、公证、TCC 身份和更新原子性；无常驻 Dock/UI。                                 |
| 本地数据库            | SQLite + rusqlite；独立 DbActor/线程；WAL                                   | Rust Core 单写入事实源；短事务、可靠 Migration、崩溃恢复和与 wx-cli SQLCipher 生态一致。           |
| 截图                  | Swift ScreenCaptureKit Bridge                                               | TCC 与 Apple API 集中在 Swift；Rust 负责调度、隐私规则、附件队列和同步。                           |
| 应用活动              | Swift NSWorkspace + AXUIElement/AXObserver + CGEventSource Bridge           | Swift 产生规范化平台事件；Rust 切分 Activity Session 并写 Event Store。                            |
| 浏览器                | Chromium Extension + Rust Native Messaging Host                             | 公开稳定获取 Tab URL/Title；Host 直接写 Rust Event Bus，不读取 Cookie、密码和表单。                |
| 位置                  | Swift Core Location Bridge                                                  | 系统授权可审计；Rust 负责频率、去重、保留和同步。                                                  |
| 文件变化              | Swift FSEvents Bridge；Rust Scope/Projection                                | 系统 API 与业务规则分离；V0 只保存 metadata，不上传正文。                                          |
| 通信                  | Rust CommunicationProvider；复用/裁剪 pandorafuture/wx-cli crates           | 直接集成 wx-keychain/wx-db/wx-monitor/wx-context 思路，避免 CLI 子进程和会话摘要轮询。             |
| WeChat Key            | Keychain 保存 KeyMaterial；被动扫描已登录进程；Active Extraction 仅维修模式 | 正常路径不退出/启动微信、不提示登录；密钥一次成功后长期复用并按数据库验证。                        |
| 云端 API              | Hono + @hono/zod-openapi + TypeScript                                       | Agent 使用稳定 REST/OpenAPI；Web 共享合同但不锁死客户端发布节奏。                                  |
| 身份                  | Better Auth + Device Pairing Token                                          | Web 使用 Session；Agent 用一次性配对换取设备凭据并存入 Keychain。                                  |
| 云数据库              | PostgreSQL + Drizzle ORM                                                    | 跨设备事实、投影、命令、更新、审计和保留任务。                                                     |
| 对象存储              | Cloudflare R2 / S3 Compatible                                               | 截图和诊断包通过预签名 URL 上传。                                                                  |
| Dashboard             | Next.js + TypeScript + TanStack Query + Tailwind + shadcn/ui                | 高密度 Web 工作台、响应式页面和统一组件。                                                          |
| 自动更新              | Sparkle 2 更新 App Bundle；UpdateCoordinator 协调 Rust/Swift/SQLite         | 统一更新签名、公证、下载和安装；升级前停止 agentd、备份 DB，失败进入恢复模式。                     |
| 可观测                | tracing + Sentry/OTel + 结构化本地日志                                      | 统一 traceId/deviceId/runId，串联 Rust、Swift Bridge、Cloud 和 Provider。                          |

## 1.4 文档事实源与变更规则

- 正文中的最终决策高于附录中的字段字典和实现示例；发现冲突时必须同一提交修正文档、Schema 和 Contract。

- 同一概念只保留一个权威定义。Event、Collector、Permission、Sync、Retention 和 Update 状态不得在多个章节各自发明。

- 重大架构调整必须新增 ADR，明确状态、上下文、被替代决策和迁移影响，不允许静默覆盖。

- 本主规格定义 V0 完整语义与终局架构；Sprint 可以选择纵向切片，但不得重定义对象含义、状态机或安全规则。

- Drizzle Schema、Rust Local Migration、OpenAPI Schema、Bridge Protocol 与本数据字典必须由 CI 做集合、枚举和兼容性检查。

# 2. 产品定位、目标用户与边界

## 2.1 产品定位

Personal Computer Agent 不是员工监控 SaaS，也不是远程控制工具。它是个人用户授权后运行在自己设备上的数字活动记录系统：把分散的应用、页面、截图、消息、文件和位置事件统一成可检索的个人时间线。

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>核心价值主张</strong></p>
<p>用户不需要回忆“我昨天在哪个窗口、哪个网站、哪段微信里看到过这件事”。系统以设备、时间、来源和关联截图为索引，重建可验证的个人数字活动轨迹。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 2.2 目标用户

| **用户类型**            | **典型任务**                           | **主要痛点**                             | **V0 付费价值**                                     |
|-------------------------|----------------------------------------|------------------------------------------|-----------------------------------------------------|
| 一人公司 / 独立创始人   | 开发、研究、沟通、内容与运营同时进行   | 上下文切换多，重要信息散落在应用和聊天中 | 统一时间线、截图回溯、微信/浏览器检索、设备在线状态 |
| 远程工作者 / 自由职业者 | 跨地点、跨工具完成客户与项目工作       | 无法回忆某时段做了什么、在哪工作         | 应用时长、浏览记录、文件和位置轨迹                  |
| 重度电脑用户            | 同时使用 IDE、浏览器、终端、聊天和文档 | 系统历史能力碎片化                       | 统一检索和个人活动 Dashboard                        |
| 个人设备运维者          | 需要远程确认电脑状态、截图和服务健康   | 不想部署复杂 RMM                         | 设备状态、远程截图命令、掉线与权限异常告警          |

## 2.3 核心使用场景

- 打开 Timeline，按日期查看当天从应用切换、浏览页面、截图、消息、文件变化到位置移动的完整事件。

- 按“VS Code + 昨天下午”筛选截图和窗口标题，恢复某次开发上下文。

- 从 Communication 查看 WeChat 会话增量记录；点击消息定位到同一时间的截图和活动。

- 在 Devices 查看 Mac 在线、睡眠、最后心跳、磁盘、Agent/Adapter 版本和权限状态。

- 在 Dashboard 下发“立即截图”命令；Agent 轮询命令、捕获并上传，Web 近实时展示结果。

- 按应用、域名和时段统计活跃时间、空闲时间和切换次数，不使用 AI 推断。

- 设备离线或合盖期间不采集；唤醒后补传本地队列，并尝试补抓已同步到本地的数据源。

## 2.4 明确不做什么

| **非目标**              | **说明**                                                                                    |
|-------------------------|---------------------------------------------------------------------------------------------|
| 不做员工隐蔽监控        | V0 只面向设备所有者或明确授权的使用者；Agent 状态、采集开关和权限必须可见。                 |
| 不做 Keylogger          | 不记录键盘按键、剪贴板连续内容、密码或表单输入。                                            |
| 不做摄像头/麦克风监控   | 不调用 Camera/Microphone；未来任何新增必须独立权限和 ADR。                                  |
| 不做完整远程桌面        | V0 不包含视频流、键鼠控制和 RustDesk 内嵌；只支持远程命令如立即截图、暂停、恢复和状态诊断。 |
| 不绕过平台保护          | 不绕过微信版本限制、TCC、SIP、验证码、登录或加密保护；不自动降级微信。                      |
| 不保证关机/睡眠期间实时 | Mac 睡眠或关机时 Agent 不运行；只能在恢复后补传和补抓已落到本地的数据。                     |
| 不做 AI 分析            | V0 不生成日报、摘要、向量检索和自动任务；数据结构为未来 V1 AI Memory 预留。                 |

# 3. 数据分类、隐私、授权与保留

## 3.1 最终隐私裁决

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终裁决</strong></p>
<p>任何数据类别必须先登记采集目的、来源 API、权限、默认开关、云端同步策略和保留期。未登记的数据默认禁止采集。凭据类数据在任何情况下不得进入 Event Payload、截图元数据、普通日志或云端业务库。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 3.2 数据分类与默认策略

| **数据类别** | **典型数据**                           | **默认采集**         | **默认同步**                      | **敏感等级** |
|--------------|----------------------------------------|----------------------|-----------------------------------|--------------|
| 设备健康     | 在线、版本、CPU、内存、磁盘、电池      | 开启                 | 秒级/周期                         | 普通         |
| 应用活动     | Bundle ID、应用名、窗口标题、活跃时长  | 开启                 | 秒级批量                          | 中           |
| 截图         | 显示器/窗口图片、触发原因、应用上下文  | 关闭，用户开启       | 异步上传                          | 高           |
| 浏览器活动   | URL、标题、域名、Tab 活跃时长          | 关闭，安装扩展后开启 | 秒级批量                          | 高           |
| 通信内容     | 会话、发送者、消息正文、消息类型       | 关闭                 | 用户选择 full/metadata/local-only | 高           |
| 文件活动     | 路径引用、文件名、扩展名、大小、动作   | 关闭                 | metadata-only                     | 高           |
| 位置         | 经纬度、精度、时间、粗略地点           | 关闭                 | 周期/事件                         | 高           |
| 凭据与登录态 | Cookie、Token、Key、密码、微信解密密钥 | 仅本地运行时需要     | 禁止同步                          | secret       |

## 3.3 权限模型

| **权限**                       | **使用模块**                | **授权入口**                         | **未授权行为**                 | **撤销后行为**             |
|--------------------------------|-----------------------------|--------------------------------------|--------------------------------|----------------------------|
| Screen Recording               | Screenshot Collector        | 首次启用截图时请求；提供系统设置跳转 | Collector=blocked              | 立即停止截图；保留历史数据 |
| Accessibility                  | Activity Collector 窗口标题 | 首次启用窗口级活动时请求             | 只记录前台应用，不记录窗口标题 | 降级为应用级活动           |
| Location Services              | Location Collector          | 用户在 Location 页面点击启用         | Collector=disabled             | 停止更新，不删除历史       |
| Full Disk Access               | WeChat/受保护文件 Adapter   | Adapter 初始化向导                   | Adapter=permission_required    | 停止读取并提示修复         |
| Login Item / Background Items  | Agent Runtime               | 安装后注册 SMAppService              | Agent 不自动启动               | 显示后台项已关闭           |
| Automation / Browser Extension | Browser/Safari Adapter      | 按浏览器单独授权                     | 降级为应用标题                 | 停止 URL 采集              |

## 3.4 排除规则与隐私模式

- 应用排除：按 Bundle ID 排除截图、窗口标题和活动详情；排除后只保留“隐私应用”占位事件或完全不记录，由用户选择。

- 域名排除：浏览器扩展在采集端匹配，不把排除域名的 URL 发给 Agent；支持精确域名与通配符。

- 目录排除：File Collector 仅监听白名单目录；隐藏文件、系统目录、.git、node_modules 和用户规则默认忽略。

- 定时隐私模式：支持工作日/时段规则和“一键暂停 15 分钟/1 小时/直到明天”。

- 锁屏与空闲：屏幕锁定、显示器睡眠或空闲超过阈值时暂停定时截图；保留设备状态事件。

- 截屏脱敏：V0 不做 OCR/AI 自动模糊；支持手工排除应用、窗口和显示器，避免伪安全。

## 3.5 保留、导出与删除

| **对象**       | **默认保留** | **可配置**           | **删除行为**                                    |
|----------------|--------------|----------------------|-------------------------------------------------|
| 原始 Event     | 90 天        | 30/90/180/365 天     | 到期后保留日级聚合，删除原始行                  |
| 截图           | 30 天        | 7/30/90/永久         | 删除对象存储与元数据；生成 tombstone 防离线复活 |
| 通信消息       | 90 天        | local-only/30/90/365 | 按会话或全部删除；搜索索引同步清理              |
| 位置点         | 90 天        | 7/30/90/365          | 删除精确点，可保留城市级聚合（需单独开关）      |
| 文件事件       | 90 天        | 30/90/180            | 删除事件，不操作用户文件                        |
| 审计与安全日志 | 180 天       | 不可低于 30 天       | 仅保留脱敏操作事实                              |

# 4. 核心领域对象与生命周期

## 4.1 六个中心

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终裁决</strong></p>
<p>Device 是运行中心，Collector 是采集中心，Event 是事实中心，Asset 是大对象中心，Projection 是查询中心，Command 是远程操作中心。禁止把所有字段都塞进 events.payload，也禁止 Dashboard 直接查询 Collector 私有表。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

| **对象**         | **回答的问题**                             | **权威边界**                   |
|------------------|--------------------------------------------|--------------------------------|
| Device 设备      | 哪台电脑、哪个 Agent 实例产生了数据？      | 设备身份、版本、在线和撤销状态 |
| Collector 采集器 | 什么能力在采集、权限是否满足、配置是什么？ | 采集开关、版本、权限与健康     |
| Event 事件       | 某个时间点发生了什么？                     | 不可变追加事实                 |
| Asset 资源       | 事件关联了哪个截图或导出文件？             | 对象存储文件、哈希和生命周期   |
| Projection 投影  | 如何高效展示应用时长、会话、地图和图表？   | 由 Event 派生、可重建          |
| Command 命令     | 用户远程要求 Agent 做什么？                | 命令、尝试、结果与幂等         |

## 4.2 领域对象关系

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>User / Workspace<br />
├── Device<br />
│ ├── CollectorInstance<br />
│ ├── PermissionSnapshot<br />
│ ├── DeviceHeartbeat<br />
│ ├── Event<br />
│ │ ├── ScreenshotAsset<br />
│ │ └── Projection Records<br />
│ ├── AgentCommand<br />
│ │ └── AgentCommandAttempt<br />
│ └── AgentUpdateEvent<br />
└── RetentionPolicy</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 4.3 Event 生命周期

| **状态**      | **含义**                       | **允许动作**                   | **退出条件**         |
|---------------|--------------------------------|--------------------------------|----------------------|
| created_local | Collector 已生成并完成本地事务 | 投影、本地查询、进入 Outbox    | Outbox 创建成功      |
| pending_sync  | 等待上传                       | 批量、压缩、等待网络           | Sync Worker 领取     |
| sending       | 当前批次发送中                 | 超时/取消/等待 ACK             | 服务端确认或失败     |
| acked         | 云端已提交并返回 change_seq    | 清理短期 payload、副本保留     | 按本地保留策略清理   |
| conflict      | 服务端拒绝 revision 或对象状态 | 保存冲突、人工或策略处理       | 产生新 Event/Command |
| dead_letter   | 超过重试上限或不可恢复         | 导出诊断、重试、丢弃（需确认） | 用户/新版本处理      |

## 4.4 Activity Session 生命周期

| **状态/事件** | **规则**                                                                      |
|---------------|-------------------------------------------------------------------------------|
| APP_FOCUSED   | 前台应用变化时关闭旧 Session 并创建新 Session；同应用窗口变化只更新标题历史。 |
| USER_IDLE     | 连续无键鼠事件超过默认 5 分钟，当前 Session 切分为 idle；不记录具体输入。     |
| USER_ACTIVE   | 空闲结束后创建新的 active Session，不把空闲时间计入应用活跃时长。             |
| SCREEN_LOCKED | 立即结束 active Session，暂停截图和位置高频更新。                             |
| SYSTEM_SLEEP  | 写入 sleep Event，flush SQLite/WAL，暂停 Worker。                             |
| SYSTEM_WAKE   | 写入 wake Event，重新检查权限、网络、Adapter 与补偿同步。                     |

## 4.5 Agent Command 生命周期

| **状态**           | **进入条件**           | **允许动作**          | **终态** |
|--------------------|------------------------|-----------------------|----------|
| queued             | Web 创建命令           | 等待 Agent 领取、取消 | —        |
| dispatched         | Agent 拉取并确认       | 执行、超时            | —        |
| running            | 本地任务已启动         | 进度、取消、失败      | —        |
| waiting_permission | 缺少权限或需要用户操作 | 打开修复 UI、取消     | —        |
| succeeded          | 结果已上传并确认       | 只读                  | 是       |
| failed             | 执行失败且不可自动恢复 | 重试生成新 Attempt    | 是       |
| cancelled          | 用户或策略取消         | 只读                  | 是       |
| expired            | TTL 到期前未领取/完成  | 只读                  | 是       |

## 4.6 领域对象总表

| **领域**     | **核心对象**                                                                                              |
|--------------|-----------------------------------------------------------------------------------------------------------|
| 身份与范围   | AuthUser, Workspace, Device, DevicePairingCode                                                            |
| Agent 与采集 | CollectorDefinition, CollectorInstance, CollectorConfig, PermissionSnapshot                               |
| 事件与投影   | Event, ActivitySession, BrowserVisit, Conversation, Message, LocationPoint, FileEvent, SystemMetricSample |
| 资源         | ObjectFile, ScreenshotAsset                                                                               |
| 同步         | SyncBatch, SyncOutboxItem, SyncCursor, SyncDeadLetter, Tombstone                                          |
| 远程命令     | AgentCommand, AgentCommandAttempt                                                                         |
| 更新         | AgentRelease, AgentReleaseArtifact, AgentUpdateEvent                                                      |
| 治理         | RetentionPolicy, AuditEvent, ExclusionRule                                                                |

**PART II 第二部分：产品信息架构与用户流程**

把领域语义落实到 Web Dashboard、首次安装、权限、时间线、截图、活动、通信和设置。

# 5. 一级信息架构与 Web 工作台

## 5.1 一级导航

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>Personal Computer Agent Web<br />
├── 1. Overview 概览<br />
├── 2. Timeline 时间线<br />
├── 3. Screenshots 截图<br />
├── 4. Activity 活动分析<br />
├── 5. Communication 通信<br />
├── 6. Browser 浏览器<br />
├── 7. Files 文件事件<br />
├── 8. Location 位置<br />
├── 9. Devices 设备<br />
└── 10. Settings 设置</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

- 一级导航按用户要回答的问题组织，不按 Collector 技术名称组织。

- Timeline 是核心入口；其他模块是对同一事件数据的专业投影和批量操作。

- V0 不增加 AI、Automation 或 Remote Control 一级导航。

## 5.2 主界面布局

| **区域**          | **功能**                                                   |
|-------------------|------------------------------------------------------------|
| 左侧导航          | 一级模块、Workspace 切换、当前设备快捷状态、全局搜索入口。 |
| 顶部 Scope Bar    | 设备、日期范围、时区、实时/历史模式、暂停采集状态。        |
| 中部主内容        | 时间线、列表、图表、地图或设置表单。                       |
| 右侧详情抽屉      | 当前事件、截图元数据、关联事件、原始字段、删除与导出。     |
| 全局任务/告警中心 | 设备离线、权限缺失、Adapter 失效、同步积压、更新失败。     |

## 5.3 全局 Scope 与 URL 规则

| **Scope**   | **URL 参数/状态**  | **规则**                                        |
|-------------|--------------------|-------------------------------------------------|
| Workspace   | workspaceId        | V0 单用户仍保留；服务端所有查询必须带 Scope。   |
| Device      | deviceId=all\|uuid | 默认 all；Screenshot/Command 必须选择具体设备。 |
| Time Range  | from/to/timezone   | API 使用 UTC ISO 8601，UI 按用户时区展示。      |
| Event Types | types\[\]          | Timeline 多选；查询参数可分享。                 |
| Live Mode   | live=1             | 通过 10 秒轮询刷新，不要求 WebSocket。          |

## 5.4 Web 页面状态规范

| **状态**           | **统一行为**                                                          |
|--------------------|-----------------------------------------------------------------------|
| Loading            | 显示骨架屏，保留筛选栏和页面尺寸；不使用全屏 Spinner。                |
| Empty              | 说明当前 Scope 无数据，并提供启用 Collector、调整时间或连接设备入口。 |
| Permission Missing | 明确指出哪个设备、哪个权限、哪个 Collector 受影响；提供修复步骤。     |
| Offline Device     | 历史数据仍可浏览；实时按钮禁用，显示最后心跳。                        |
| Partial Data       | 某些 Collector 关闭或失败时用可见 Banner 标记，不把缺失数据当作 0。   |
| Error              | 显示错误码、可恢复动作和诊断 ID；不暴露内部路径或 Token。             |

# 6. 首次安装、设备配对与权限流程

## 6.1 首次安装流程

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>下载签名安装包<br />
→ 安装 /Applications/PersonalComputerAgent.app<br />
→ 打开最小 Setup Window<br />
→ 登录 Web / 输入一次性配对码<br />
→ 注册后台 Agent<br />
→ 逐项解释并请求权限<br />
→ 选择 Collector 与同步范围<br />
→ 完成健康检查<br />
→ 自动打开 Web Dashboard</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 6.2 启动向导步骤

| **步骤** | **最小输入**                          | **系统输出**                           | **可跳过**                         |
|----------|---------------------------------------|----------------------------------------|------------------------------------|
| 设备配对 | 网页登录或 8 位一次性码               | device_id、设备凭据写入 Keychain       | 否                                 |
| 后台运行 | 用户批准 Login Item                   | SMAppService registered                | 否；可稍后手动启动但不符合常驻目标 |
| 应用活动 | Accessibility 授权                    | 应用+窗口活动                          | 可，降级应用级                     |
| 截图     | Screen Recording 授权、频率、排除应用 | Screenshot Collector                   | 可以                               |
| 浏览器   | 安装 Extension、Native Messaging      | URL/Title/Domain 事件                  | 可以                               |
| 通信     | 选择 Adapter 与同步模式               | WeChat/其他数据源状态                  | 可以                               |
| 文件     | 选择目录                              | FSEvents 白名单                        | 可以                               |
| 位置     | Location 授权与精度                   | Location Collector                     | 可以                               |
| 完成检查 | —                                     | 权限、版本、网络、数据库、同步健康报告 | 否                                 |

## 6.3 设备配对合同

- Web 生成 pairing_code，TTL 默认 10 分钟，只能使用一次。

- Agent 使用 pairing_code、device_public_key、platform、arch 和 agent_version 调用 /v1/devices/pair。

- 服务端返回 device_id、device_access_token 与 refresh_secret；Agent 仅把秘密写入 Keychain。

- 配对完成后 pairing_code 立即失效；Dashboard 显示设备指纹、系统版本和最近 IP 粗略地区。

- 撤销设备后所有 Token 失效；本地 Agent 进入 unpaired，仅允许导出和重新配对。

## 6.4 权限修复与最小 UI

| **触发**                             | **Setup/Repair UI 行为**                                   |
|--------------------------------------|------------------------------------------------------------|
| 首次权限请求                         | 只解释当前 Collector 的目的，不一次性弹出全部系统权限。    |
| 权限被撤销                           | Dashboard 显示修复按钮；Agent 打开最小窗口并跳转系统设置。 |
| WeChat Adapter 需要 Full Disk Access | 显示影响、风险和兼容版本；禁止静默重签微信。               |
| 更新失败                             | 提供重试、导出日志、退出 Agent 和重新安装。                |
| 数据库修复模式                       | 禁止采集和同步；提供备份、完整性检查和恢复。               |

## 6.5 安装验收

- 全新 macOS 用户能在 10 分钟内完成配对、后台注册、至少两个 Collector 授权和首条事件同步。

- 关闭 Setup Window 后无 Dock 图标；Agent 继续运行。

- 注销登录并重新登录后 Agent 自动启动，Dashboard 30 秒内显示 Online。

- 撤销任一权限后对应 Collector 停止，其他 Collector 不受影响。

# 7. Overview 工作台

## 7.1 模块定义

| **子模块**       | **详细功能**                                                    |
|------------------|-----------------------------------------------------------------|
| 设备状态         | 在线/离线/睡眠、Agent 版本、最后同步、权限缺失、同步积压。      |
| 今日活跃         | 活跃时长、空闲时长、首次/最后活动、应用切换次数。               |
| Top Applications | 按活跃秒数排序；排除空闲和锁屏。                                |
| 采集覆盖         | 各 Collector 开关、健康、最后事件、错误码；缺失数据不显示为 0。 |
| 最近时间线       | 最近 20 条重要事件；可跳转 Timeline。                           |
| 截图速览         | 最近 8 张非隐私截图；点击预览。                                 |
| 告警与待处理     | 权限、设备掉线、Adapter 失效、Outbox 堵塞、更新。               |
| 快捷命令         | 立即截图、暂停采集、恢复采集、刷新健康、检查更新。              |

## 7.2 指标口径

| **指标**     | **计算规则**                                                               |
|--------------|----------------------------------------------------------------------------|
| 活跃时长     | ActivitySession where state=active 的 duration 之和；不从 Event 数量推算。 |
| 空闲时长     | USER_IDLE 到 USER_ACTIVE 的区间；锁屏和睡眠单列。                          |
| App 使用时长 | 前台 active Session 按 bundle_id 聚合。                                    |
| 切换次数     | APP_FOCUS_CHANGED 且 previous_bundle_id != current_bundle_id。             |
| 在线         | last_heartbeat_at 距当前 \<= 45 秒；45–180 秒为 stale；\>180 秒 offline。  |
| 同步积压     | local outbox pending/sending 数量，由心跳上报摘要。                        |

# 8. Timeline 与 Screenshot Explorer

## 8.1 Timeline 页面目的

Timeline 是系统核心入口，回答“在指定设备和时间范围内发生了什么”。它不是日志 dump：同类低价值事件需要合并展示，但原始 Event 仍可在详情中查看。

## 8.2 Timeline 组件树

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>TimelinePage<br />
├── GlobalScopeBar<br />
├── TimelineFilterBar<br />
│ ├── DateRangePicker<br />
│ ├── DeviceSelect<br />
│ ├── EventTypeMultiSelect<br />
│ ├── AppDomainFilter<br />
│ └── SearchInput<br />
├── TimelineVirtualList<br />
│ ├── DayDivider<br />
│ ├── ActivitySessionItem<br />
│ ├── ScreenshotItem<br />
│ ├── MessageItem<br />
│ ├── BrowserVisitItem<br />
│ ├── FileEventItem<br />
│ └── LocationItem<br />
└── EventDetailDrawer</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 8.3 Timeline 聚合规则

| **原始事件**              | **展示聚合**                                       |
|---------------------------|----------------------------------------------------|
| 连续 WINDOW_TITLE_CHANGED | 同一应用 30 秒内只显示最后标题；详情保留标题历史。 |
| 系统指标                  | Timeline 默认隐藏；仅异常或用户主动勾选时显示。    |
| FileEvent burst           | 同目录 10 秒内合并为“新增 12 / 修改 4”。           |
| Browser visits            | 同域名连续访问合并为 Session；URL 明细在详情中。   |
| Screenshots               | 作为时间线卡片或活动项附件，不重复显示两次。       |
| Messages                  | 同会话 60 秒内可折叠，但每条 Message 保持独立。    |

## 8.4 Screenshot Explorer

| **能力** | **详细要求**                                                 |
|----------|--------------------------------------------------------------|
| 视图     | 按时间网格、按应用分组、按 Timeline 嵌入三种视图。           |
| 筛选     | 设备、日期、应用、触发类型、显示器、收藏/删除。              |
| 预览     | 原图缩放、元数据、前后截图、关联活动和消息。                 |
| 触发类型 | timer/app_switch/manual/command/communication_event。        |
| 去重     | 先比较内容 hash；可选 pHash 阈值跳过近似相同截图。           |
| 删除     | 单张、批量、日期范围；服务端生成 tombstone，并删除对象存储。 |
| 下载     | 原图、按日期 ZIP、带 manifest 的导出包。                     |

## 8.5 Screenshot 采集显示规则

- 截图卡片必须显示设备、时间、应用、窗口标题、触发类型和同步状态。

- 被排除应用产生的截图不得上传；如果在捕获时发现前台应用进入排除列表，取消当前截图任务。

- 多显示器默认分别保存，Dashboard 可按 Capture Group 同时展示；不强制拼接超宽图片。

- 设备缩放、Retina scale 和旋转写入 metadata，避免预览比例错误。

# 9. Activity、Browser 与工作时长

## 9.1 Activity 页面

| **区域**     | **功能**                                                              |
|--------------|-----------------------------------------------------------------------|
| 时间总览     | 活跃、空闲、锁屏、睡眠四类时间，不用“工作效率”标签。                  |
| 应用排行     | 总时长、占比、Session 数、平均 Session、切换进入次数。                |
| 时间分布     | 小时/日/周；按设备和应用筛选。                                        |
| Session 明细 | 开始、结束、应用、窗口标题、URL（如果有）、关联截图。                 |
| 分类         | 用户手工把应用归类为开发/研究/沟通/娱乐/隐私；V0 不使用 AI 自动分类。 |

## 9.2 Browser 页面

| **能力**   | **V0 规则**                                                            |
|------------|------------------------------------------------------------------------|
| 支持浏览器 | Chrome、Edge、Brave 等 Chromium；Safari 仅通过活动窗口标题降级。       |
| 采集来源   | Extension 只发送当前活跃 Tab 的 URL、标题、domain、tabId hash 和时间。 |
| 禁止采集   | Cookie、LocalStorage、密码、表单、页面正文、下载内容。                 |
| 排除规则   | 扩展端先过滤，排除数据不发送给 Native Host。                           |
| 时长       | Tab 激活 + 浏览器前台 + 用户非 idle 三者同时满足才计入。               |
| 搜索       | URL、标题和域名前缀；不做页面全文索引。                                |

## 9.3 活跃时间确定性规则

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>active_time = foreground_app<br />
AND user_not_idle<br />
AND screen_unlocked<br />
AND system_awake<br />
<br />
browser_active_time = active_time<br />
AND foreground_app_is_browser<br />
AND extension_reports_active_tab</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

- 空闲阈值默认 300 秒，可配置 60–1800 秒。

- 窗口标题为空或权限不足不影响应用时长；只影响明细。

- 合盖、睡眠和关机期间不生成虚构 Session。

# 10. Communication、Files、Location、Devices 与 Settings

## 10.1 Communication

| **区域**     | **功能**                                                               |
|--------------|------------------------------------------------------------------------|
| Adapter 状态 | Provider、版本、兼容状态、最后同步、延迟、错误码。                     |
| 会话列表     | 名称、类型、最后消息、消息数、同步模式；不显示未能确认的“未读”假数据。 |
| 消息查看     | 文本、图片/文件元数据、发送者、时间；附件按 Adapter 能力显示。         |
| 搜索         | 会话、发送者、文本、时间和消息类型。                                   |
| 关联时间线   | 跳转同一时段活动和截图。                                               |
| 隐私         | full / metadata-only / local-only 三种同步策略。                       |

## 10.2 Files

| **能力** | **规则**                                                       |
|----------|----------------------------------------------------------------|
| 目录     | 用户白名单；默认 Desktop、Documents、Downloads 均不自动开启。  |
| 事件     | create/update/delete/rename；rename 尽量由 inode/fileID 关联。 |
| 字段     | 相对路径或路径 hash、文件名、扩展名、大小、时间；不读取正文。  |
| 忽略     | .git、node_modules、缓存、临时文件和用户自定义 glob。          |
| 展示     | 按目录/扩展名/动作统计，并关联当时应用和截图。                 |

## 10.3 Location

| **能力**   | **规则**                                                         |
|------------|------------------------------------------------------------------|
| 当前位置   | 显示最后点、精度半径、更新时间和来源。                           |
| 历史       | 点列表和地图路径；低精度点以圆形范围展示。                       |
| 采集频率   | 默认 30 分钟；网络变化或显著位置变化触发；不低于 5 分钟。        |
| 逆地理编码 | 云端/Provider 可选；原始坐标与地点名称分离，避免 Provider 锁定。 |
| 隐私       | 默认关闭；支持仅上传城市级网格。                                 |

## 10.4 Devices

| **页面能力** | **详细要求**                                           |
|--------------|--------------------------------------------------------|
| 设备列表     | 名称、平台、系统、在线、版本、最后心跳、最后活动。     |
| 设备详情     | Collectors、权限、同步队列、系统指标、存储和更新状态。 |
| 远程命令     | 立即截图、暂停/恢复、刷新健康、检查更新、导出诊断。    |
| 撤销         | 撤销 Token；保留云端历史；本地进入重新配对。           |
| 命名         | 用户可改显示名；device_key 不变。                      |

## 10.5 Settings

| **设置组** | **内容**                                         |
|------------|--------------------------------------------------|
| Collectors | 开关、频率、排除规则、同步模式、保留期。         |
| Privacy    | 暂停、隐私时段、敏感应用/域名/目录、精确位置。   |
| Data       | 导出、删除、云端存储、截图质量、网络策略。       |
| Device     | Agent 版本、后台项、权限、日志、诊断、更新通道。 |
| Account    | Workspace、登录、安全会话、撤销设备。            |

## 10.6 导出与删除流程

- 导出必须生成 manifest，记录设备、时区、筛选条件、表版本和文件 hash。

- 删除请求先从业务查询和搜索移除，再生成 tombstone，最后物理删除对象存储；离线设备不得复活。

- 删除设备不等于删除历史数据；两者必须分别确认。

**PART III 第三部分：Agent Runtime 与 Collector**

定义 Rust Core Runtime、Swift macOS Bridge、Collector、静默通信 Provider、同步、命令和自动更新。

# 11. 总体系统与部署架构

## 11.1 能力边界

| **运行位置**           | **负责能力**                                                                   | **不负责**                                               |
|------------------------|--------------------------------------------------------------------------------|----------------------------------------------------------|
| Web Dashboard          | 查询、筛选、展示、配置、命令、设备管理                                         | 不直接连接 Agent、本地文件、数据库或第三方应用进程       |
| Cloud API              | 鉴权、设备、事件、投影、命令、更新、签名 URL                                   | 不读取本地未上传数据和本地凭据                           |
| Cloud Worker/Cron      | 聚合、保留期、对象清理、更新策略                                               | 不执行 macOS 系统采集                                    |
| Rust Agent Core        | Runtime、Collector、Event Bus、SQLite、Outbox、Sync、Command、Provider、Health | 不直接实现 Apple UI/TCC API，不承载日常 Dashboard        |
| Swift macOS Bridge     | Apple API、权限探测、Power/Workspace/FSEvents、截图与位置                      | 不写业务表、不调用 Cloud、不决定保留/同步策略            |
| Browser Extension/Host | 活跃 Tab metadata、Native Messaging                                            | 不读取 Cookie、密码、表单正文                            |
| Communication Provider | 读取已授权本地数据源、维护 Cursor、标准化消息                                  | 不修改第三方应用、不发送消息、不自动重签或重启第三方应用 |

## 11.2 典型任务流

| **任务**          | **执行路径**                                                                                                                                  |
|-------------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| 应用切换          | Swift NSWorkspace/AX Bridge → Rust ActivityCollector → Event+Session → SQLite transaction → Outbox → Cloud → Timeline。                       |
| 定时截图          | Rust Scheduler/privacy guard → Swift ScreenCaptureKit Bridge → local file → Event+Attachment Outbox → R2 → Cloud metadata。                   |
| 立即截图          | Web Command → Cloud queued → Rust Command Worker → Swift Capture → upload → succeeded。                                                       |
| WeChat 静默初始化 | Rust Provider Supervisor → Keychain verify → 检测已登录 WeChat → passive Mach scan → SQLCipher verify → Keychain → ACTIVE；未登录则静默等待。 |
| WeChat 增量       | session.db/WAL change → SessionTracker → per-talker after_sort_seq → Message Event → local Event Store → sync policy。                        |
| 设备睡眠/唤醒     | Swift power notification → Rust flush/checkpoint/suspend；wake → recheck Bridge/network/providers → catch-up。                                |
| 自动更新          | Sparkle download → UpdateCoordinator safe point → stop Rust/Bridge → SQLite backup → install → relaunch → migration/health。                  |

## 11.3 云端技术栈

| **层**          | **建议技术**                                      | **职责**                               |
|-----------------|---------------------------------------------------|----------------------------------------|
| Web             | Next.js + TypeScript + TanStack Query + shadcn/ui | Dashboard、路由、筛选、命令和设置。    |
| API             | Hono + Zod OpenAPI                                | Agent REST、Web API、Auth Middleware。 |
| Auth            | Better Auth                                       | 用户 Session、设备配对、撤销。         |
| Database        | PostgreSQL + Drizzle ORM                          | 业务事实源、投影、命令、更新和审计。   |
| Object Storage  | Cloudflare R2                                     | 截图、诊断包和导出。                   |
| Background Jobs | Cloudflare Queues/Cron 或 BullMQ（长任务时）      | 保留期、聚合、清理、通知。             |
| Observability   | Sentry + OpenTelemetry                            | API、Dashboard 和 Agent 诊断关联。     |

# 12. macOS Agent 与进程设计

## 12.1 Rust Core Runtime + Swift macOS Bridge

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终裁决</strong></p>
<p>V0 macOS Agent 使用 Swift 单语言实现。Collector 与系统 API 高度依赖 macOS，加入 Rust 只会增加 FFI、构建、签名、崩溃栈和更新复杂度。跨平台 Agent 属于 V2，届时复用 Event/Collector Contract，而不是提前引入双语言 Runtime。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 12.2 App Bundle 与后台注册

- 安装产物是签名、公证的 PersonalComputerAgent.app，内嵌 Rust agentd、Swift PlatformBridge、Native Messaging Host 与更新组件，统一放入 /Applications。

- Setup App 设置 LSUIElement=true；默认不出现在 Dock 和 Cmd+Tab。Rust agentd 与 Bridge 无独立用户界面。

- Swift Setup/Repair App 负责一次性配对、系统权限入口、Sparkle 更新、注册 Rust LaunchAgent 和主动诊断；正常采集不要求它保持运行。

- macOS 13+ 使用 SMAppService.agent(plistName:) 注册 Rust agentd；旧系统仅在兼容版本中回退签名 LaunchAgent plist。

- Rust agentd 与 Swift Bridge 均运行在登录用户会话中；不使用 root LaunchDaemon。需要 task_for_pid 的 WeChat 被动扫描仅在系统能力探测通过时执行，不尝试提升权限。

## 12.3 进程模型

进程边界固定为“Rust 管业务与状态，Swift 管 Apple API”。Rust agentd 通过 0600 Unix Domain Socket 与 PlatformBridge 通信；消息采用 length-prefixed MessagePack/JSON，必须携带 protocol_version、request_id、capability、deadline 和 error_code。Bridge 崩溃只使依赖该能力的 Collector 进入 degraded，Rust Core、SQLite、同步和其他 Provider 继续运行。

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><strong>PersonalComputerAgent.app</strong><br />
<strong>├── Swift Setup/Repair App（按需）</strong><br />
<strong>│ ├── Device Pairing / Permission Entry</strong><br />
<strong>│ ├── SMAppService Registration</strong><br />
<strong>│ └── Sparkle Update Coordinator</strong><br />
<strong>├── Rust agentd（LaunchAgent，常驻）</strong><br />
<strong>│ ├── Runtime Supervisor / Collector Registry</strong><br />
<strong>│ ├── Event Bus / Projection / SQLite DbActor</strong><br />
<strong>│ ├── Sync / Command / Health / Update Handshake</strong><br />
<strong>│ └── Communication Provider Supervisor</strong><br />
<strong>├── Swift PlatformBridge（按需或常驻轻进程）</strong><br />
<strong>│ ├── ScreenCaptureKit / Accessibility / NSWorkspace</strong><br />
<strong>│ ├── Core Location / FSEvents / Power / TCC</strong><br />
<strong>│ └── Versioned Bridge Protocol</strong><br />
<strong>├── Rust Native Messaging Host（按浏览器启动）</strong><br />
<strong>└── Optional Repair Provider（LLDB Active Extractor，仅显式维修）</strong></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 12.4 Runtime 状态机

| **状态**           | **进入条件**                        | **允许动作**                                                | **退出条件**             |
|--------------------|-------------------------------------|-------------------------------------------------------------|--------------------------|
| unpaired           | 无设备凭据或被撤销                  | 本地诊断、打开配对 UI；不得启动云同步                       | 成功配对                 |
| initializing       | Rust agentd 启动                    | 日志、Keychain、SQLite/Migration、Bridge 握手、Registry     | running/degraded/repair  |
| waiting_permission | 已启用 Collector 缺少 TCC           | 其他 Collector 继续；静默等待权限变化；仅主动设置页显示状态 | 权限满足或关闭 Collector |
| running            | Core/DB/Bridge/身份正常             | 采集、Provider、同步、命令、心跳                            | sleep/update/fatal       |
| degraded           | 部分 Bridge/Collector/Provider 失败 | 其余能力继续；后台退避和能力探测                            | 恢复或 repair            |
| sleeping           | 系统即将睡眠                        | flush、checkpoint、停止 timer/Provider polling              | wake                     |
| updating           | UpdateCoordinator 进入安全点        | 冻结副作用、停止 Bridge/agentd、备份/安装                   | 重启或恢复               |
| repair             | Migration、DB、签名、协议不兼容     | 只读诊断、恢复、重新安装；不主动干扰第三方 App              | 修复成功                 |
| stopped            | 用户关闭后台项/卸载                 | 无                                                          | 重新注册/启动            |

## 12.5 启动顺序

1.  Rust agentd 初始化 tracing、Crash Marker、instance lock 和 runtime version。

2.  从 Keychain 读取 device token、Provider KeyMaterial 引用和 Bridge shared secret；缺失设备凭据则进入 unpaired。

3.  启动专用 DbActor，打开 SQLite，设置 WAL/foreign_keys/busy_timeout，执行不可变 Rust Migration 链。

4.  执行 integrity_check/foreign_key_check 和关键查询 Smoke Test；异常进入 repair。

5.  加载 Rust Collector/Provider Registry、云端 desired config 与本地 capability probe；启动并握手 Swift PlatformBridge。

6.  启动 Event Bus、Projection、Outbox、Sync Worker、Command Worker、Heartbeat 和 Provider Supervisor。

7.  启动 enabled 且 capability satisfied 的 Collector；缺权限或数据源未就绪时保持静默等待，不弹窗。

8.  写入 AGENT_STARTED、BRIDGE_READY 和 PROVIDER_DISCOVERY_STARTED Event，上传首个心跳。

## 12.6 Sleep / Wake / Lid Close

- Swift Bridge 发送 willSleep：Rust 停止新截图、Provider watcher 和命令副作用，提交 SQLite 事务，WAL checkpoint，写 sleep Event。

- 睡眠期间不采集、不上传、不执行命令，Dashboard 显示 sleeping 或 offline。

- Swift Bridge 发送 didWake：Rust 写 wake Event，等待网络和 Bridge 稳定，重新检查 TCC、浏览器扩展、WeChat 进程、KeyMaterial 与数据库 Cursor，运行 catch-up。

- 补抓只保证“最终进入本地数据源”的记录；关机期间未同步到 Mac 微信数据库的消息无法保证。

## 12.7 Agent 本地安全基线

| **区域**   | **控制**                                                                                                                        |
|------------|---------------------------------------------------------------------------------------------------------------------------------|
| 凭据       | device token、Bridge secret、WeChat KeyMaterial 和 Adapter secret 只进 Keychain；SQLite 只保存 credential_ref、版本和验证时间。 |
| 本地 IPC   | UDS 路径位于 Application Support，目录 0700、Socket 0600；双向 nonce/secret 握手；拒绝未知 protocol_version。                   |
| 文件权限   | 应用数据目录 0700；截图和 DB 0600；不把解密后的完整 WeChat 数据库写入公共 /tmp。                                                |
| 子进程     | 正常运行不依赖 shell/CLI；维修工具仅白名单路径、固定参数、超时和显式模式。                                                      |
| 第三方应用 | 不得默认退出、启动、重签、注入或修改 WeChat；被动读取失败时静默等待或标记 capability unavailable。                              |
| 网络       | 仅 HTTPS；证书错误直接失败；Cloud token 和对象 URL 最小期限。                                                                   |
| 日志       | 结构化 JSON；消息正文、Key、路径默认脱敏；用户主动导出诊断时仍过滤 Secret。                                                     |
| 更新       | 验证 Sparkle EdDSA、代码签名、公证、Rust/Bridge 协议兼容和 DB Schema 窗口。                                                     |

# 13. Collector Framework

## 13.1 Collector Contract

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><strong>Rust Contract</strong><br />
<strong>#[async_trait]</strong><br />
<strong>pub trait Collector: Send + Sync {</strong><br />
<strong>fn definition(&amp;self) -&gt; &amp;'static CollectorDefinition;</strong><br />
<strong>async fn initialize(&amp;mut self, ctx: CollectorContext) -&gt; Result&lt;()&gt;;</strong><br />
<strong>async fn start(&amp;mut self, sink: EventSink) -&gt; Result&lt;()&gt;;</strong><br />
<strong>async fn pause(&amp;mut self, reason: PauseReason) -&gt; Result&lt;()&gt;;</strong><br />
<strong>async fn resume(&amp;mut self) -&gt; Result&lt;()&gt;;</strong><br />
<strong>async fn stop(&amp;mut self) -&gt; Result&lt;()&gt;;</strong><br />
<strong>async fn health(&amp;self) -&gt; CollectorHealth;</strong><br />
<strong>async fn update_config(&amp;mut self, cfg: CollectorConfig) -&gt; Result&lt;()&gt;;</strong><br />
<strong>}</strong><br />
<br />
<strong>pub struct CollectorDefinition {</strong><br />
<strong>pub key: &amp;'static str,</strong><br />
<strong>pub version: &amp;'static str,</strong><br />
<strong>pub required_capabilities: &amp;'static [CapabilityKey],</strong><br />
<strong>pub supported_event_types: &amp;'static [EventType],</strong><br />
<strong>pub risk_level: RiskLevel,</strong><br />
<strong>pub execution: ExecutionLocation,</strong><br />
<strong>}</strong></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 13.2 Collector 职责边界

| **Collector 负责**                | **Collector 不负责**        |
|-----------------------------------|-----------------------------|
| 访问一个明确的数据源              | 直接调用 Cloud API          |
| 根据权限和配置采集                | 决定保留期和商业权限        |
| 产生标准 Event 和 Attachment 引用 | 自行建云端表                |
| 提供健康状态、错误分类和版本      | 读取其他 Collector 私有状态 |
| 执行本地去重和节流                | 生成跨模块业务分析          |

## 13.3 Collector 状态

| **状态**            | **含义**                       |
|---------------------|--------------------------------|
| disabled            | 用户关闭或策略关闭。           |
| permission_required | 缺少系统权限。                 |
| initializing        | 加载资源或探测 Provider。      |
| running             | 持续产生事件。                 |
| paused              | 隐私模式、睡眠或用户暂停。     |
| degraded            | 部分字段/能力不可用但能继续。  |
| unsupported         | 系统/应用版本不兼容。          |
| error               | 不可恢复错误，等待用户或更新。 |

## 13.4 Collector Registry 与配置

- Collector Definition 是代码注册表；Collector Instance 是某设备上的运行状态。

- 配置变更由 Cloud 保存为 desired_config_revision，Agent 拉取后校验并写 local config；成功后上报 applied_revision。

- 高风险配置（全文通信、精确位置、截图频率 \< 60 秒）必须在首次产品级授权或用户主动设置中确认；确认后正常启停与数据源等待均静默执行，Web 不能越过本地授权扩大范围。

- 未知字段拒绝应用并返回 CONFIG_SCHEMA_UNSUPPORTED，避免旧 Agent 静默忽略安全设置。

## 13.5 Backpressure 与健康

| **场景**           | **处理**                                                            |
|--------------------|---------------------------------------------------------------------|
| Outbox \> 10,000   | 暂停低优先级 System/Window 事件，保留应用切换、命令结果和安全事件。 |
| 磁盘可用 \< 2GB    | 停止截图和附件下载，保留 metadata，发 DISK_SPACE_LOW。              |
| 云端 429           | 遵守 Retry-After，批次缩小，Collector 不受阻塞。                    |
| Collector 连续失败 | 指数退避；超过阈值进入 degraded/error。                             |
| Adapter 子进程卡死 | 超时终止，记录 stderr 摘要，重新启动。                              |

## 13.6 V0 Collector 清单

| **Collector Key**    | **默认**   | **执行位置/能力**                            | **主要 Event**                                                       |
|----------------------|------------|----------------------------------------------|----------------------------------------------------------------------|
| system               | ON         | Rust + Swift Power/Network Bridge            | system.metric_sampled, system.sleep, system.wake, network.offline, network.online |
| activity             | ON         | Swift Workspace/AX Bridge + Rust Sessionizer | activity.app_focused, activity.window_changed, activity.idle_started |
| screenshot           | OFF        | Rust Scheduler + Swift ScreenCaptureKit      | screen.screenshot_created                                            |
| browser              | OFF        | Extension + Rust Native Host                 | browser.visit_started, browser.visit_ended                           |
| file                 | OFF        | Swift FSEvents Bridge + Rust Scope           | file.created, file.updated, file.deleted, file.renamed               |
| location             | OFF        | Swift Core Location Bridge                   | location.updated                                                     |
| communication.wechat | 授权后静默 | Rust WeChatProvider + Keychain + SQLCipher   | communication.message_received, communication.provider_ready         |

# 14. Screenshot、Activity 与 System Collector

## 14.1 Screenshot Collector 最终规则

| **维度** | **规则**                                                                      |
|----------|-------------------------------------------------------------------------------|
| 触发     | timer、app_switch_stable、manual_local、remote_command、communication_event。 |
| 默认频率 | 5 分钟；最低 60 秒；空闲、锁屏、睡眠时不执行。                                |
| 捕获范围 | 每个显示器单独；用户可选单显示器、前台窗口或全部显示器。                      |
| 格式     | HEIC 或 JPEG/WebP（根据支持）；metadata 记录 codec/quality/scale。            |
| 压缩     | 最长边默认 1920 用于云端；可保留本地原尺寸。                                  |
| 去重     | SHA-256 完全去重；可选 pHash 近似去重。                                       |
| 排除     | Bundle ID、窗口标题正则、显示器、隐私时段。                                   |
| 上传     | 创建 Attachment Outbox，先拿 signed URL，再上传，再提交 Event。               |

## 14.2 Screenshot Payload

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>{<br />
"capture_group_id": "uuid",<br />
"display_id": "stable-display-id",<br />
"width": 1920,<br />
"height": 1200,<br />
"scale": 2.0,<br />
"trigger": "timer",<br />
"foreground_bundle_id": "com.microsoft.VSCode",<br />
"window_title": "schema.ts — project",<br />
"object_file_id": "uuid",<br />
"content_hash": "sha256",<br />
"privacy_rule_revision": 17<br />
}</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 14.3 Activity Collector

| **数据源**                                     | **用途**               | **降级**                       |
|------------------------------------------------|------------------------|--------------------------------|
| NSWorkspace.didActivateApplicationNotification | 前台应用切换           | 核心路径，不依赖 Accessibility |
| AXUIElement / AXObserver                       | 前台窗口标题、窗口变化 | 无权限时字段为空               |
| CGEventSource.secondsSinceLastEventType        | 用户 idle 计时         | 读取时长，不记录事件内容       |
| 屏幕锁定/睡眠通知                              | 结束 Session、暂停截图 | 核心路径                       |

## 14.4 Activity Session 生成规则

- Session 以 bundle_id + active/idle 状态切分；窗口标题变化不创建新 Session，写 title_history 或 Window Event。

- 同一应用重新获得焦点必须创建新 Session，避免跨应用时间连续。

- Agent 崩溃恢复时，未关闭 Session 以 last_heartbeat_at 截断并标记 recovered=true。

- 窗口标题属于高敏感数据；用户可设置只记录应用名。

## 14.5 System Collector

本轮仅完成 System CPU/Memory 与 Disk 纵向切片；Battery/Power、Network 留待后续。

| **指标**        | **频率**            | **Event / payload**                                                                                                                                                                                                 |
|-----------------|---------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Agent heartbeat | 15 秒               | 版本、状态、Collector 摘要、Outbox 数量。                                                                                                                                                                            |
| CPU/Memory      | 启动立即一次，30 秒 | `system.metric_sampled`，`metric_group=cpu_memory`；`sample_window_ms`、`logical_cpu_count`，以及 host 的 `cpu_usage_percent`、`memory_total_bytes`、`memory_used_bytes` 和 Agent 的 `cpu_usage_percent`、`memory_resident_bytes`。 |
| Disk            | 启动立即一次，5 分钟 | `system.metric_sampled`，`metric_group=disk`；`scope=pca_data_volume`、`total_bytes`、`available_bytes`、`used_percent`、`low_space`、`low_space_threshold_bytes=2147483648`、`warning_code`。                               |

主机和 Agent CPU 都归一化为 0–100。Disk 只定位 PCA Data 所在卷；Event 不输出路径、卷名、文件系统、进程 PID、命令行、环境变量、SSID、电池或网络数据。低空间状态变化使用 `system.health_changed`，正常采样不中断。

实现固定使用 `sysinfo = 0.33.1`，`default-features = false`，仅启用 `system` 与 `disk`；所有阻塞刷新收口到单一有界 sampler actor，不占用 async executor 线程。

# 15. Browser、File 与 Location Collector

## 15.1 Browser Extension 架构

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>Chromium Extension<br />
activeTab / tabs.onActivated / windows.onFocusChanged<br />
↓ Native Messaging (JSON lines)<br />
Native Messaging Host<br />
↓ Unix domain socket / XPC<br />
Agent BrowserCollector<br />
↓ browser.* Events</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

- Native Host manifest 由安装器注册；只接受签名扩展 ID。

- 扩展端维护排除域名，禁止把被排除 URL 发到 Native Host。

- 消息包含 extension_version 和 schema_version；不兼容时拒绝。

- 浏览器不是前台或用户 idle 时，Tab 不计活跃时长。

## 15.2 Browser Event Payload

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>{<br />
"browser": "chrome",<br />
"profile_key": "local-hash",<br />
"tab_session_id": "uuid",<br />
"url": "https://github.com/...",<br />
"title": "Repository",<br />
"domain": "github.com",<br />
"started_at": "...",<br />
"ended_at": "...",<br />
"duration_seconds": 382,<br />
"incognito": false<br />
}</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 15.3 File Collector

| **规则**   | **说明**                                                     |
|------------|--------------------------------------------------------------|
| 白名单目录 | 用户显式选择；保存 security-scoped bookmark 或授权引用。     |
| FSEvents   | 按目录流读取；事件必须去抖、合并和处理 dropped flag。        |
| 文件身份   | 优先 file resource identifier/inode + volume；路径用于展示。 |
| 重命名     | 可确认时产生 file.renamed；否则 delete+create。              |
| 内容边界   | 不读取正文、不上传文件；hash 仅在用户开启去重时计算。        |
| 路径同步   | 默认只同步相对路径或 hash；完整绝对路径需显式开启。          |

## 15.4 Location Collector

- Info.plist 必须包含 macOS Location 使用说明。

- 用户在 Web 开启后，Agent 本地 UI 再确认；云端不能远程静默开启精确位置。

- 每个点保存 horizontal_accuracy、source_information 和 reduced_accuracy 标记。

- 精度 \> 5km 的点默认不画路径，只显示粗略区域。

- 位置变化小于 accuracy 半径时可跳过上传。

# 16. Communication Adapter 与静默 WeChat Provider

## 16.1 Communication Adapter 总原则

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终裁决</strong></p>
<p>通信能力不是一个通用“读取所有通知”的 Collector。macOS 没有面向普通第三方 App 的稳定公开 API 用于读取其他应用的完整通知内容；V0 必须使用应用级 Adapter、官方 API 或用户导入。任何通过私有 Notification Center 数据库抓取的方案不进入稳定产品。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 16.2 Adapter Contract

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><strong>Rust Contract</strong><br />
<strong>#[async_trait]</strong><br />
<strong>pub trait CommunicationProvider: Send + Sync {</strong><br />
<strong>fn definition(&amp;self) -&gt; &amp;'static ProviderDefinition;</strong><br />
<strong>async fn probe(&amp;self, ctx: ProbeContext) -&gt; ProviderProbe;</strong><br />
<strong>async fn initialize(&amp;mut self, ctx: ProviderContext) -&gt; Result&lt;InitOutcome&gt;;</strong><br />
<strong>async fn list_conversations(&amp;self, page: PageCursor) -&gt; Result&lt;Page&lt;ConversationDto&gt;&gt;;</strong><br />
<strong>async fn fetch_after(&amp;self, cursor: ProviderCursor) -&gt; Result&lt;MessageBatch&gt;;</strong><br />
<strong>async fn run(&amp;mut self, sink: EventSink, shutdown: CancellationToken) -&gt; Result&lt;()&gt;;</strong><br />
<strong>async fn health(&amp;self) -&gt; ProviderHealth;</strong><br />
<strong>async fn shutdown(&amp;mut self) -&gt; Result&lt;()&gt;;</strong><br />
<strong>}</strong></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 16.3 WeChat Provider 最终定位

- V0 将 WeChat 实现为 Rust CommunicationProvider，不作为 Agent 核心启动条件；Provider 未就绪、未登录或不兼容时，其他采集、同步和 Dashboard 继续。

- 正式实现不通过反复执行 wechat-cli 子进程。Provider 复用/裁剪 pandorafuture/wx-cli 的 keychain、decrypt、db、monitor、context 设计，并通过本项目 Port 隔离第三方代码。

- 用户只在产品安装/设置时一次性授权通信采集范围；之后初始化静默进行。Agent 不保存微信密码、Cookie 或开放平台凭据，KeyMaterial 只保存在 macOS Keychain。

- 近实时链路为 session.db/session.db-wal 变化检测 → SessionTracker → 每会话 after_sort_seq 查询 → Message Event；macOS 默认 2–3 秒 mtime polling，可按能力切换 fsnotify。

- Rust Provider 按 account_id + talker 维护 sort_seq Cursor，并以 server_id/sort_seq/talker 生成稳定去重键；不依赖 CLI last_check.json 或未读状态。

## 16.4 静默发现、Key 获取与初始化状态机

| **状态**               | **后台行为**                               | **用户可见行为**         |
|------------------------|--------------------------------------------|--------------------------|
| disabled               | 不探测、不读取                             | 无                       |
| waiting_source         | 低频检测 WeChat 进程、账号目录和 DB 活跃度 | 无                       |
| checking_stored_key    | 读取 Keychain 并验证数据库                 | 无                       |
| passive_scanning       | 扫描当前已登录进程并交叉验证 Key           | 无                       |
| verifying_database     | 只读打开 core/shard DB，检查 schema/cursor | 无                       |
| active                 | 启动 WAL/session watcher 和消息增量流      | Dashboard 数据自然出现   |
| capability_unavailable | 指数退避重试；保持其他 Agent 功能          | 仅用户主动打开诊断时可见 |
| unsupported            | 停止高频尝试，等待 Provider/Agent 更新     | 仅用户主动打开诊断时可见 |

| **阶段**          | **静默规则**                                                                                                            |
|-------------------|-------------------------------------------------------------------------------------------------------------------------|
| 授权门禁          | 只有本设备所有者已在产品设置中启用 communication.wechat 时才运行 Provider；未启用不探测、不扫描。                       |
| Stored Key        | 优先从 Keychain 加载 account-scoped RawKey/EncKeyPair，并验证 session/contact/message 数据库；成功直接 ACTIVE。         |
| Source Discovery  | 检测 WeChat PID、账号目录、session.db/WAL 活跃度。未运行或未登录时保持 waiting_source，不产生前台 UI。                  |
| Passive Key Scan  | 仅附着当前已登录进程，扫描预派生 EncKey+Salt，按数据库 Salt 和 HMAC/SQLCipher 查询交叉验证；不 kill/open WeChat。       |
| Version Probe     | 不只按版本字符串硬阻断；执行 crypto parameter probe、目录 probe 和 schema probe。已知不兼容才 unsupported。             |
| 4.1.12 策略       | 标记 experimental；先放宽版本白名单并运行只读兼容 Spike。任何 Schema/crypto 不匹配都失败关闭，不猜字段。                |
| Active Extraction | LLDB PBKDF2 Hook 会重启 WeChat，仅保留在 developer/manual repair mode；V0 后台自动流程永不调用。                        |
| Re-sign/SIP       | Agent 不自动关闭 SIP、不自动重签 WeChat、不提升权限。capability 不满足时静默等待或在用户主动诊断页展示。                |
| 成功条件          | 至少能只读打开 session.db、contact.db 和一个 message shard；Cursor/去重 Contract Test 通过后写 Keychain 并进入 ACTIVE。 |

## 16.5 SQLCipher 读取、实时检测与补抓

- Agent 启动早于微信或微信未登录时，Provider 进入 waiting_source；后台低频探测进程、账号目录和 session.db 活跃度，不弹窗、不启动微信、不要求登录。条件满足后自动继续。

- 睡眠/关机期间不实时读取。唤醒后等待 WeChat 数据库和 WAL 稳定，再按 talker sort_seq Cursor 补抓；手机端已读不会影响补抓。

- 禁止把 unread_count 或 Session summary 视为消息事实源；真实新增消息必须从 message shard 按 Cursor 拉取。

- 图片、语音、文件可能晚于消息记录下载；先写消息 Event 和 attachment_pending，文件到达后再生成 attachment_resolved Event。

- 微信升级导致 Key/Schema 失效时，先验证 Keychain KeyMaterial，再尝试对当前已登录进程静默被动扫描；失败后进入 capability_unavailable 并低频重试，不自动重启、重签或打扰用户。

## 16.6 同步模式

| **模式**      | **云端内容**                                             | **适用**                  |
|---------------|----------------------------------------------------------|---------------------------|
| full          | 会话、发送者、正文和附件 metadata；Secret 永不上传       | 用户明确授权 Web 全文查看 |
| metadata_only | 会话 hash、消息类型、时间、长度和方向，不上传正文/显示名 | 只分析通信活动量          |
| local_only    | 云端只上报 Provider 健康、计数和延迟                     | 最高隐私场景              |

## 16.7 wx-cli、wechat-cli 与其他工具边界

| **工具/代码源**       | **实现特点**                                                                       | **V0 决策**                                                      |
|-----------------------|------------------------------------------------------------------------------------|------------------------------------------------------------------|
| pandorafuture/wx-cli  | Rust workspace；SQLCipher 直接读；session/WAL watcher；per-talker cursor；REST/SSE | 主要技术参考与可复用代码源；通过 internal Provider Port 重新封装 |
| huohuoer/wechat-cli   | Python + C memory scanner；完整解密缓存；new-messages 为会话摘要差异               | 只用于 fixture、兼容对照和历史查询验证；不作为实时底座           |
| CipherTalk            | 历史导入、查看和分析产品                                                           | 可做 Import Adapter；不进入实时 Runtime                          |
| Active LLDB Extractor | 重启微信并 Hook PBKDF2，能取得 RawKey                                              | 仅 developer/manual repair；自动后台流程禁用                     |

# 17. Sync Engine、远程命令与自动更新

## 17.1 Local Outbox

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终裁决</strong></p>
<p>业务 Event、Projection 更新和 sync_outbox 必须在同一 SQLite 事务提交。Collector 成功的定义是“本地事实和 Outbox 均已落盘”，不是“HTTP 已发送”。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 17.2 Push 流程

| **步骤** | **Agent**                                                        | **Cloud**                           | **失败处理**        |
|----------|------------------------------------------------------------------|-------------------------------------|---------------------|
| Batch    | 按 priority、occurred_at、dependency 排序，最多 200 Event 或 1MB | —                                   | 批次过大拆分        |
| Send     | 附 device_id、batch_id、schema_version、idempotency_key          | 鉴权、Schema、租户和重复检查        | 网络错误指数退避    |
| Commit   | 等待                                                             | 事务写 Event、Projection/change_seq | 字段错误逐项返回    |
| ACK      | 更新 outbox=acked、server_change_seq                             | 返回 accepted/duplicate/rejected    | 只重试 retryable 项 |
| Cleanup  | 按本地保留清理 payload/附件                                      | —                                   | 不影响云端事实      |

## 17.3 Attachment 上传

9.  Agent 计算 hash、size、mime，调用 /v1/objects/prepare。

10. 服务端命中相同 hash 可返回 existing_object_file_id；否则返回 signed PUT URL。

11. Agent 直传 R2，校验响应和 ETag。

12. Agent 调用 /v1/objects/complete；服务端 HEAD 校验后创建 ObjectFile。

13. 包含 object_file_id 的 Event 才进入最终 ACK；孤儿上传由清理任务删除。

## 17.4 Retry 分类

| **错误**         | **重试** | **策略**                                  |
|------------------|----------|-------------------------------------------|
| 网络超时/5xx/429 | 是       | 指数退避 + jitter；尊重 Retry-After。     |
| 401/设备撤销     | 否       | 刷新一次；失败进入 unpaired。             |
| Schema 不兼容    | 否       | 提示升级 Agent。                          |
| Payload 字段错误 | 否       | dead_letter + 诊断。                      |
| 对象上传中断     | 是       | 重新 prepare；不假设 signed URL 仍有效。  |
| 存储空间不足     | 条件     | 停止附件，保留 metadata，用户清理后继续。 |

## 17.5 远程命令

| **Command Type**   | **参数**                | **副作用/权限**                         |
|--------------------|-------------------------|-----------------------------------------|
| capture_screenshot | display/window/quality  | 需要 Screen Recording；产生截图 Asset。 |
| pause_collection   | collector/all, duration | 本地立即执行；记录 Audit。              |
| resume_collection  | collector/all           | 不自动开启缺权限 Collector。            |
| refresh_health     | none                    | 无副作用。                              |
| check_update       | channel                 | 只检查，不自动安装。                    |
| export_diagnostics | time_range              | 生成脱敏包并上传，用户可下载。          |

## 17.6 自动更新

- Sparkle Feed 使用 stable/beta/internal 三通道；安装包同时声明 Rust agentd、Swift Bridge、Bridge Protocol 和 DB Schema 兼容范围，V0 对外只开放 stable。

- 后台检查和下载可以静默；安装在安全点执行。新增系统权限或高风险 Adapter 变更必须提示。

- UpdateCoordinator 固定流程：停止新命令 → flush Outbox → WAL checkpoint → SQLite backup → 等待/取消子进程 → install → relaunch。

- 有 running command 或 Adapter init 时默认稍后更新；用户可选择取消任务并更新。

- 更新后首次启动执行 Migration、integrity_check、Collector smoke test；失败进入 Repair，不带半迁移数据库运行。

**PART IV 第四部分：数据、API 与安全架构**

定义 PostgreSQL、SQLite、Object Storage、同步合同、API 和字段级数据边界。

# 18. 领域数据、标识与事实源边界

## 18.1 标识、时间与多范围约束

| **规则**   | **最终设计**                                         | **原因**                          |
|------------|------------------------------------------------------|-----------------------------------|
| 主键       | 应用侧 UUIDv7；PostgreSQL uuid，SQLite TEXT          | 离线创建、时间有序、跨库一致。    |
| Workspace  | 所有云端业务表带 workspace_id                        | 未来团队隔离；V0 单用户也不省略。 |
| Device     | 设备数据必须带 device_id                             | 多设备查询和撤销。                |
| 时间       | UTC；API ISO 8601；SQLite integer milliseconds       | 时区与排序一致。                  |
| 不可变事实 | Event、Heartbeat、Metric、Audit、Update Event 追加式 | 防止历史漂移。                    |
| 软删除     | 可同步实体 deleted_at + Tombstone                    | 离线设备不复活。                  |
| 幂等       | batch/command/object/update 使用 idempotency_key     | 重试不重复。                      |
| 扩展字段   | Provider 原始数据用 jsonb_ref；核心字段结构化        | 避免 JSON 垃圾桶。                |

## 18.2 云端与本地事实源

| **数据对象**                  | **Cloud**      | **Local**                  | **规则**                  |
|-------------------------------|----------------|----------------------------|---------------------------|
| 用户/Workspace/设备撤销       | 权威           | 缓存                       | 云端最终裁决。            |
| 原始 Event                    | 跨设备权威     | 创建源与短期缓存           | 本地 ACK 后可按策略清理。 |
| Activity/Browser/Message 投影 | Dashboard 权威 | 本地工作集                 | 可由 Event 重建。         |
| 截图文件                      | 已上传对象权威 | 上传前本地事实             | hash 关联；可选择只本地。 |
| Collector 配置                | desired 权威   | applied 权威               | 通过 revision 握手。      |
| 凭据/微信 Key/浏览器密钥      | 禁止           | Keychain/Provider 私有存储 | 不进入同步链路。          |
| Agent Command                 | 命令权威       | 执行缓存                   | 结果由云端保存。          |

## 18.3 数据分区

| **PostgreSQL Schema** | **职责**                               | **关键表**                                                                                                              |
|-----------------------|----------------------------------------|-------------------------------------------------------------------------------------------------------------------------|
| auth                  | 用户和 Session                         | auth_users, auth_sessions, auth_accounts                                                                                |
| app                   | Workspace、设备、配对、Collector、权限 | workspaces, devices, device_pairing_codes, collector_instances, permission_snapshots                                    |
| events                | 原始事件与投影                         | events, activity_sessions, browser_visits, conversations, messages, location_points, file_events, system_metric_samples |
| assets                | 对象与截图                             | object_files, screenshots                                                                                               |
| commands              | 远程命令                               | agent_commands, agent_command_attempts                                                                                  |
| sync                  | 批次、游标和 tombstone                 | sync_batches, device_sync_cursors, tombstones                                                                           |
| release               | Agent 发布与更新                       | agent_releases, agent_release_artifacts, agent_update_events                                                            |
| governance            | 保留、排除和审计                       | retention_policies, exclusion_rules, audit_events                                                                       |

# 19. Cloud PostgreSQL Schema

## 19.1 云端核心表组

| **表组**  | **表**                                                                                                          | **权威语义**               |
|-----------|-----------------------------------------------------------------------------------------------------------------|----------------------------|
| 设备      | devices, device_pairing_codes, device_heartbeats                                                                | 设备身份、配对与在线历史。 |
| Collector | collector_instances, collector_configs, permission_snapshots                                                    | 期望配置与实际状态。       |
| 事件      | events                                                                                                          | 不可变事实流。             |
| 投影      | activity_sessions, browser_visits, conversations, messages, location_points, file_events, system_metric_samples | Dashboard 查询模型。       |
| 资源      | object_files, screenshots                                                                                       | 截图与诊断包对象。         |
| 命令      | agent_commands, agent_command_attempts                                                                          | 远程命令计划和执行。       |
| 同步      | sync_batches, device_sync_cursors, tombstones                                                                   | 幂等、Pull 游标和删除。    |
| 更新      | agent_releases, agent_release_artifacts, agent_update_events                                                    | 发布、产物和遥测。         |
| 治理      | retention_policies, exclusion_rules, audit_events                                                               | 隐私与审计。               |

## 19.2 索引计划

| **表**            | **关键索引**                                                                                                           | **用途**          |
|-------------------|------------------------------------------------------------------------------------------------------------------------|-------------------|
| events            | (workspace_id, device_id, occurred_at desc); (workspace_id, event_type, occurred_at desc); UNIQUE(device_id, event_id) | Timeline 与幂等。 |
| activity_sessions | (device_id, started_at desc); (device_id, bundle_id, started_at)                                                       | 应用排行和时段。  |
| screenshots       | (device_id, captured_at desc); (workspace_id, content_hash)                                                            | 网格与去重。      |
| messages          | (conversation_id, sent_at desc); GIN search_tsv                                                                        | 会话与全文搜索。  |
| browser_visits    | (device_id, started_at desc); (workspace_id, domain, started_at)                                                       | 历史和域名统计。  |
| location_points   | GiST geography; (device_id, captured_at desc)                                                                          | 地图范围和时间。  |
| agent_commands    | (device_id, status, created_at); UNIQUE(idempotency_key)                                                               | Agent 轮询。      |

## 19.3 事件分区与保留

- events 和 metric 表在数据量达到阈值后按月分区；V0 可先使用普通表，但 Repository 不依赖物理分区。

- Retention Job 先删除投影和对象，再按 Event 保留规则清理；删除顺序必须尊重审计和 tombstone。

- Dashboard 聚合表不存无法解释的 AI 结果；所有统计可回溯到原始 Session/Event。

# 20. Local SQLite Schema

## 20.1 最终本地数据库裁决

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终裁决</strong></p>
<p>SQLite 只由 Agent Service 进程访问。Setup UI、Native Messaging Host 和 Adapter 子进程不得直接打开数据库；全部通过 Agent 内部 Actor/IPC。WAL、foreign_keys、busy_timeout=5000、synchronous=NORMAL。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 20.2 本地表组

| **类别**    | **表**                                                                                                                        | **同步规则**                       |
|-------------|-------------------------------------------------------------------------------------------------------------------------------|------------------------------------|
| Meta        | local_meta, schema_migrations, agent_state                                                                                    | 设备本地，不同步或只上报摘要。     |
| Collector   | collector_configs, collector_states, permission_states                                                                        | 配置 revision 双向，状态上行。     |
| Event       | events_local                                                                                                                  | 创建源；进入 Outbox。              |
| Projection  | activity_sessions_local, browser_visits_local, messages_local, location_points_local, file_events_local, system_metrics_local | 短期缓存；可重建。                 |
| Assets      | screenshots_local, attachments_local                                                                                          | 上传前本地事实，ACK 后按策略清理。 |
| Sync        | sync_outbox, sync_attachment_outbox, sync_cursors, sync_dead_letters, tombstones_local                                        | 可靠同步。                         |
| Commands    | agent_commands_local, command_attempts_local                                                                                  | 执行和恢复点。                     |
| Diagnostics | diagnostic_events, update_state                                                                                               | 脱敏本地诊断。                     |

## 20.3 Migration 与备份

- 每个 Migration 有 id、checksum、app_version、started_at、completed_at、status；已发布 Migration 永久冻结。

- 涉及 Schema 更新前执行 WAL checkpoint 和 SQLite Backup API；不能只复制 .db 忽略 wal/shm。

- Migration 采用 expand → backfill → contract；旧 Agent 遇到高于 max_supported_schema_version 的 DB 必须拒绝写入。

- 迁移完成运行 integrity_check、foreign_key_check 和关键查询 smoke test。

# 21. Sync Contract、冲突、删除与保留

## 21.1 Push Contract

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>POST /v1/agent/sync/events<br />
{<br />
"deviceId": "uuid",<br />
"batchId": "uuid",<br />
"agentVersion": "1.0.0",<br />
"schemaVersion": 4,<br />
"events": [ ... ],<br />
"idempotencyKey": "sha256"<br />
}<br />
<br />
200<br />
{<br />
"batchId": "uuid",<br />
"accepted": [{"eventId":"...","changeSeq":123}],<br />
"duplicates": ["..."],<br />
"rejected": [{"eventId":"...","code":"...","retryable":false}]<br />
}</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 21.2 Pull / Config / Command

- Agent 不需要 Pull 全量业务 Event；只 Pull desired Collector config、Agent command、retention/update policy 和撤销状态。

- 配置使用 monotonic config_revision；Agent 只在完整应用后上报 applied_revision。

- Command 使用 cursor/created_at + ID 排序；Agent 领取后服务端原子设置 dispatched。

## 21.3 冲突策略

| **对象**                | **策略**                                                           |
|-------------------------|--------------------------------------------------------------------|
| Event                   | 不可变，重复 ID 返回 duplicate；内容不同则 SECURITY_ID_COLLISION。 |
| Collector Config        | 云端 desired revision 最终；本地高风险确认可以拒绝并返回 reason。  |
| Device Name             | Last-write-wins + Audit。                                          |
| Retention Policy        | 云端最终；缩短保留期需二次确认并创建删除任务。                     |
| Message Attachment 状态 | 后到的数据生成补充 Event，不修改原 Event；Projection 可更新。      |
| Command                 | idempotency_key 唯一；重试创建 Attempt，不复制副作用。             |

## 21.4 Tombstone 删除合同

14. Web 创建 Delete Request，服务端把对象从默认查询移除。

15. 生成 tombstone(change_seq) 并下发设备，阻止本地缓存重新上传。

16. 后台清理 Projection、Event payload、对象存储和搜索索引。

17. Audit 只保留匿名删除事实，不保留正文。

# 22. API、Object Storage 与 Adapter 合同

## 22.1 API 分组

| **分组**         | **关键 Endpoint**                                                                                  |
|------------------|----------------------------------------------------------------------------------------------------|
| Auth/Pairing     | POST /v1/device-pairing-codes; POST /v1/devices/pair; POST /v1/devices/token/refresh               |
| Agent            | POST /v1/agent/heartbeat; POST /v1/agent/sync/events; GET /v1/agent/config; GET /v1/agent/commands |
| Objects          | POST /v1/objects/prepare; POST /v1/objects/complete; DELETE /v1/objects/:id                        |
| Timeline         | GET /v1/timeline; GET /v1/events/:id                                                               |
| Screenshots      | GET /v1/screenshots; GET /v1/screenshots/:id; DELETE /v1/screenshots                               |
| Activity         | GET /v1/activity/summary; GET /v1/activity/sessions; GET /v1/applications                          |
| Communication    | GET /v1/conversations; GET /v1/messages                                                            |
| Devices/Commands | GET /v1/devices; GET /v1/devices/:id; POST /v1/devices/:id/commands                                |
| Settings         | GET/PUT collector-configs; retention-policies; exclusion-rules                                     |
| Updates          | GET /v1/agent-update/policy; POST /v1/agent-update/events                                          |

## 22.2 API 统一错误结构

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>{<br />
"error": {<br />
"code": "PERMISSION_REQUIRED",<br />
"message": "Screen Recording permission is required.",<br />
"requestId": "req_...",<br />
"retryable": false,<br />
"details": {"permission":"screen_recording"}<br />
}<br />
}</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 22.3 API 设计规则

- Agent API 版本化且向后兼容；Dashboard 内部接口也不得依赖未发布字段。

- 所有列表 cursor pagination；时间范围查询限制最大跨度，导出走异步任务。

- Agent 请求使用 Device Bearer Token，Web 使用 Better Auth Session；不得混用。

- 对象下载使用短时 signed URL；服务端鉴权后生成。

- OpenAPI Schema 是 API 合同事实源，Rust Agent Client 与 TypeScript SDK 由生成器或 Contract Test 校验。

## 22.4 Object Storage Key

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>workspace/{workspace_id}/device/{device_id}/<br />
screenshots/{yyyy}/{mm}/{dd}/{screenshot_id}.{ext}<br />
diagnostics/{yyyy}/{mm}/{diagnostic_id}.zip<br />
exports/{export_id}/manifest.json</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

**PART V 第五部分：设计系统与工程治理**

冻结 Dashboard 视觉、仓库边界、Code Agent 规则、测试、性能、安全和实施路线。

# 23. Web Dashboard UI Design System

## 23.1 视觉定位

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>最终定案</strong></p>
<p>Dashboard 使用高密度、低装饰、时间线优先的开发者工具风格。UI 不模仿员工监控产品的“评分/警告”心智，也不使用 AI 助手式聊天作为主界面。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 23.2 Design Token

| **Token 层** | **示例**                                                 | **用途**                       |
|--------------|----------------------------------------------------------|--------------------------------|
| 基础         | spacing.4, radius.12, font.size.13                       | 原始值，不在业务页面直接使用。 |
| 语义         | color.canvas, surface, text.primary, border.subtle       | 页面和组件默认依赖。           |
| 状态         | status.online, warning, permission, privacy              | 必须配合图标/文案。            |
| 组件         | sidebar.item.active, timeline.rail, screenshot.selection | 由语义 Token 派生。            |

## 23.3 字体与尺寸

| **项目** | **规范**                                                                                      |
|----------|-----------------------------------------------------------------------------------------------|
| 字体     | -apple-system, BlinkMacSystemFont, PingFang SC, Microsoft YaHei, Noto Sans CJK SC, sans-serif |
| 字号     | 11 元数据；12 标签；13 菜单/表格；14 输入/卡片标题；16 区块；20 页面标题。                    |
| 圆角     | 6 标签；8 按钮；10 输入；12 卡片；16 Drawer/Dialog。                                          |
| 间距     | 4px 网格；常用 4/8/12/16/20/24/32。                                                           |
| 图标     | 统一线性体系，16/18px，线宽约 1.5。                                                           |

## 23.4 Timeline Item 视觉合同

| **元素** | **要求**                                       |
|----------|------------------------------------------------|
| 时间轨   | 固定 72px；显示本地时间，Hover 显示 UTC。      |
| 类型图标 | 使用 EventType 注册表，不在页面硬编码颜色。    |
| 标题     | 应用/会话/文件名；隐私事件用统一占位。         |
| 摘要     | 最多两行；正文详情进入 Drawer。                |
| 附件     | 截图缩略图、文件 icon、位置 mini map；懒加载。 |
| 状态     | sync/permission/error 只在异常时显示。         |

## 23.5 可访问性与性能

- 核心筛选、时间线导航、截图预览和命令必须可键盘操作。

- 状态不能只靠颜色；在线、缺权限和隐私暂停均配文字。

- Timeline 使用虚拟列表；截图缩略图响应式尺寸和 lazy loading。

- 主题支持 system/light/dark；V0 不做第三方皮肤。

- 所有页面需要 Loading、Empty、Partial、Error、Permission 和 Offline 状态。

# 24. Monorepo、Code Agent 与工程治理

## 24.1 最终仓库目录

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><strong>PersonalComputerAgent/</strong><br />
<strong>├── apps/</strong><br />
<strong>│ ├── web-dashboard/ # Next.js</strong><br />
<strong>│ ├── cloud-api/ # Hono</strong><br />
<strong>│ └── cloud-worker/ # retention/aggregation</strong><br />
<strong>├── agent/</strong><br />
<strong>│ ├── core/ # Rust agentd binary</strong><br />
<strong>│ ├── collectors/ # Rust collectors/sessionizers</strong><br />
<strong>│ ├── event-store/ # Event/Projection/Outbox/DbActor</strong><br />
<strong>│ ├── sync-engine/ # push/ack/retry/command</strong><br />
<strong>│ ├── provider-wechat/ # Rust CommunicationProvider</strong><br />
<strong>│ ├── native-messaging-host/ # Rust browser host</strong><br />
<strong>│ └── tests/</strong><br />
<strong>├── platform/</strong><br />
<strong>│ └── macos/</strong><br />
<strong>│ ├── SetupApp/ # Swift LSUIElement/SMAppService/Sparkle</strong><br />
<strong>│ ├── PlatformBridge/ # Swift Apple API bridge</strong><br />
<strong>│ ├── BridgeProtocol/ # schema + generated models</strong><br />
<strong>│ └── Tests/</strong><br />
<strong>├── crates/</strong><br />
<strong>│ ├── domain/ # state machines/contracts</strong><br />
<strong>│ ├── db-local/ # rusqlite schema/migrations</strong><br />
<strong>│ ├── provider-contracts/</strong><br />
<strong>│ ├── wx-decrypt-adapter/</strong><br />
<strong>│ ├── wx-db-adapter/</strong><br />
<strong>│ └── test-contracts/</strong><br />
<strong>├── packages/</strong><br />
<strong>│ ├── contracts/ # Event/API/enum JSON Schema</strong><br />
<strong>│ ├── db-cloud/ # Drizzle schema/migrations</strong><br />
<strong>│ ├── domain-ts/</strong><br />
<strong>│ ├── ui/</strong><br />
<strong>│ └── config/</strong><br />
<strong>├── browser-extension/</strong><br />
<strong>├── docs/{data,api,adr,runbooks}/</strong><br />
<strong>├── AGENTS.md</strong><br />
<strong>├── CLAUDE.md</strong><br />
<strong>├── ARCHITECTURE.md</strong><br />
<strong>├── SECURITY.md</strong><br />
<strong>└── PERFORMANCE.md</strong></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 24.2 逻辑依赖方向

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>Web UI → Web Application Services → Domain Contracts → API Client<br />
Cloud API → Application Use Cases → Domain → Ports → Infrastructure<br />
macOS Collector → Event Contract → Local Store/Outbox<br />
<br />
禁止反向依赖：<br />
UI/Collector 不得导入 Provider SDK 或数据库实现。</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 24.3 模块依赖机器约束

| **禁止依赖**                                 | **机器实现**                                                           |
|----------------------------------------------|------------------------------------------------------------------------|
| Web UI → Drizzle/数据库                      | ESLint no-restricted-imports + package exports                         |
| Cloud Domain → Hono/Provider SDK             | dependency-cruiser / eslint boundaries                                 |
| Rust Collector → Cloud Client                | Cargo crate graph + architectural tests；只依赖 EventSink/Command Port |
| Swift PlatformBridge → SQLite/Cloud/Provider | Swift package boundaries；Bridge 只依赖 protocol/generated DTO         |
| Rust Domain → AppKit/ScreenCaptureKit        | cargo-deny + crate allowlist；Apple Framework 仅 platform/macos        |
| Core Agent → wx-cli CLI JSON                 | 只依赖 CommunicationProvider Port 和内部 wx adapter crates             |
| Provider → Setup UI                          | Provider 只返回状态/错误；不得触发弹窗、打开或退出第三方 App           |
| 跨包深层路径                                 | Cargo visibility/package exports；禁止 ../internal 和未公开 module     |

## 24.4 工具链

| **领域**   | **工具/配置**                                                                | **强制要求**                                                                    |
|------------|------------------------------------------------------------------------------|---------------------------------------------------------------------------------|
| Rust       | stable, rustfmt, clippy -D warnings, cargo-nextest                           | 状态机、Provider、DB、Sync 全部严格类型；禁止无界 blocking；unsafe 需安全说明。 |
| Swift      | Swift 6 strict concurrency, rustfmt/clippy + SwiftFormat/SwiftLint           | 只实现 macOS Bridge/Setup；Actor 隔离、Sendable 和协议兼容检查。                |
| TypeScript | strict, noUncheckedIndexedAccess, exactOptionalPropertyTypes                 | Cloud/Web 全部继承统一 tsconfig。                                               |
| 依赖/安全  | cargo-deny, cargo-audit, npm audit/Dependabot, CodeQL                        | 许可证、漏洞和来源异常阻止发布。                                                |
| 数据库     | Rust migration runner + Drizzle Kit + replay verification                    | 生产禁止 push；Cloud/Local 独立 Migration，语义由 Contract 对齐。               |
| Bridge     | JSON Schema/MessagePack fixture + generated Rust/Swift models                | 协议 breaking change 必须提升 protocol_version 并保留兼容窗口。                 |
| 测试       | cargo nextest, Swift Testing, Vitest, Playwright                             | 状态机、Cursor、Bridge 和 Migration 必须覆盖失败路径。                          |
| CI         | format → lint → build → unit → contract → migration → E2E → package/notarize | Rust、Swift、Cloud、Web 和签名产物全部通过。                                    |

## 24.5 文件体系

| **文件**        | **职责**                         | **重复规则**                   |
|-----------------|----------------------------------|--------------------------------|
| AGENTS.md       | 共同架构、命令、禁止事项和 DoD   | 根目录唯一权威；局部只写增量。 |
| CLAUDE.md       | 导入 AGENTS 并补 Claude 特有流程 | 不得复制整套规则。             |
| ARCHITECTURE.md | 进程、模块和依赖方向             | 事实源。                       |
| SECURITY.md     | 威胁模型、数据分类、权限和响应   | 由测试/CI 映射。               |
| PERFORMANCE.md  | 预算与验证命令                   | 由 benchmark 映射。            |
| docs/adr        | 高成本/不可逆决策                | Supersede，不静默覆盖。        |

# 25. 测试、可观测性、性能与安全门禁

## 25.1 测试策略

| **测试层**                  | **内容**                                                                                              |
|-----------------------------|-------------------------------------------------------------------------------------------------------|
| Rust Domain Unit            | Agent/Collector/Provider 状态机、Event、Cursor、幂等、保留、权限和错误分类。                          |
| Rust Integration            | rusqlite Migration/WAL、Outbox、Retry、Command、Keychain adapter、SQLCipher fixture。                 |
| Rust/Swift Unit/Integration | Bridge Mapper、TCC probe、ScreenCaptureKit、AX、FSEvents、sleep/wake、SMAppService。                  |
| Bridge Contract             | Rust/Swift 双端 fixture、版本协商、超时、重连、崩溃与旧协议兼容。                                     |
| WeChat Provider             | stored-key verify、waiting_source、passive scan、session/WAL change、per-talker cursor、重复/漏消息。 |
| Adapter Contract            | 浏览器 Native Messaging、Provider error mapping、对象存储和地图。                                     |
| Cloud API                   | Auth、Workspace Scope、Batch Sync、Command 幂等、Object 完成。                                        |
| Web Component/E2E           | Timeline、截图、权限状态、设备命令、删除导出。                                                        |
| Update                      | 签名、Feed、Rust/Bridge 停止、DB Migration、失败恢复。                                                |
| Failure Injection           | 断网、429、磁盘低、agentd/Bridge crash、WeChat 未登录/升级、权限撤销。                                |

## 25.2 可观测性

| **维度** | **要求**                                                                                     |
|----------|----------------------------------------------------------------------------------------------|
| 日志     | JSON；timestamp、deviceId、collectorKey、eventId、batchId、commandId；敏感字段脱敏。         |
| 指标     | Collector event rate/error、Outbox depth、sync latency、command success、screenshot upload。 |
| 追踪     | request_id / batch_id / command_id 串联 Agent 与 Cloud。                                     |
| 崩溃     | Agent Crash Marker、上次状态、未完成命令和 DB checkpoint。                                   |
| 诊断包   | 用户主动导出：版本、权限状态、脱敏日志、Migration、health，不包含截图/消息正文默认。         |

## 25.3 性能预算

| **区域**        | **V0 预算**                          | **验证**                    |
|-----------------|--------------------------------------|-----------------------------|
| Agent 空闲 CPU  | \< 1% 平均；无采集时接近 0           | Activity Monitor + signpost |
| Agent 空闲内存  | \< 120MB                             | 进程采样                    |
| 启动到心跳      | \< 5 秒（不等待微信/浏览器）         | 启动 trace                  |
| Activity 延迟   | \< 2 秒                              | 事件 fixture                |
| 远程截图        | 命令到 Web 可见 p95 \< 15 秒（在线） | E2E                         |
| SQLite 常用写入 | p95 \< 20ms                          | os_signpost/query timer     |
| Sync 批次       | 200 Event 或 1MB；支持背压           | load test                   |
| Timeline API    | 普通 24h 查询 p95 \< 300ms           | APM                         |
| Web Timeline    | 10k 项虚拟滚动无明显卡顿             | Playwright/performance      |

## 25.4 安全威胁门禁

| **威胁**               | **控制**                                                   |
|------------------------|------------------------------------------------------------|
| 恶意网页调用本地 Agent | V0 不开放 loopback HTTP；Native Messaging 仅允许签名扩展。 |
| Token 泄漏             | Keychain、日志脱敏、短期 access token 和设备撤销。         |
| Provider 子进程注入    | 固定可执行路径、参数数组、超时、无 shell。                 |
| 截图越权               | 本地 TCC + Collector config + Web command 三层校验。       |
| 跨 Workspace 查询      | 服务端 Scope Middleware + FK/查询条件；测试覆盖。          |
| 离线设备复活删除数据   | Tombstone 和 Event ID 拒绝。                               |
| 更新供应链             | Sparkle 签名、代码签名、公证、Feed 权限隔离。              |

## 25.5 Definition of Done

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>任何 Code Agent 只有在以下项目全部满足后才能声称“完成”<br />
rustfmt、clippy -D warnings、cargo nextest、Rust/Swift/TypeScript 编译、Bridge/Provider Contract Test、Cloud/Local Migration 空库与升级回放、依赖边界、签名构建、性能/安全影响说明、相关文档/ADR/数据字典更新、无新增未归属 TODO。未执行的验证必须明确列出，不能用“应该可以”代替证据。</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

# 26. 实施路线、风险与 Sprint

## 26.1 V0 目标与核心假设

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>V0 核心假设</strong></p>
<p>用户是否愿意安装一个透明、可控、低资源的后台 Agent，以换取可检索的个人电脑活动时间线和远程状态 Dashboard。V0 成功判据是完整走通“安装授权 → 连续采集 → 离线恢复 → Web 查看 → 远程截图 → 删除导出”，不是 Collector 数量越多越好。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## 26.2 V0 必做范围

| **领域**       | **V0 必做**                                                                                                                                 |
|----------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| Agent          | 签名 App、Rust agentd、Swift PlatformBridge、SMAppService、Keychain、rusqlite、日志、sleep/wake。                                           |
| 核心 Collector | System、Activity、Screenshot；Rust 调度，Swift 提供 Apple API。                                                                             |
| 扩展 Collector | Chromium Browser、File、Location；WeChat Provider experimental 但架构完整。                                                                 |
| WeChat         | 静默 waiting_source、stored-key verify、passive scan、SQLCipher read、session/WAL watcher、per-talker Cursor；自动 Active Extraction 不做。 |
| 同步           | Event/Attachment Outbox、Batch ACK、Retry、Command Polling。                                                                                |
| 云端           | Better Auth、Device Pairing、PostgreSQL、R2、OpenAPI。                                                                                      |
| Dashboard      | Overview、Timeline、Screenshots、Activity、Communication、Browser、Files、Location、Devices、Settings。                                     |
| 治理           | 产品级授权、排除、保留、导出、删除、审计；正常数据源等待无弹窗。                                                                            |
| 更新           | Sparkle stable 通道、Rust/Bridge 协调、DB 备份、Migration、失败恢复。                                                                       |

## 26.3 明确延后

| **能力**                       | **目标版本**                      |
|--------------------------------|-----------------------------------|
| AI 日报、语义搜索、个人 Memory | V1                                |
| Windows/Linux Agent            | V2                                |
| RustDesk/远程桌面/Computer Use | V2                                |
| 自动回复或发送微信消息         | 不在当前路线；需独立合规评审      |
| 完整 Safari URL Collector      | V1/V1.5                           |
| E2EE 多设备密钥体系            | V1                                |
| 团队员工监控/管理员策略        | 不做或单独产品线                  |
| 手机端系统监控                 | 不做 V0；Android/iOS 能力边界不同 |

## 26.4 七个两周 Sprint

| **Sprint** | **主题**                 | **主要交付**                                                                             | **退出门禁**                                                               |
|------------|--------------------------|------------------------------------------------------------------------------------------|----------------------------------------------------------------------------|
| S0         | 工程基线                 | Cargo/Xcode/pnpm Monorepo、CI、Contracts、AGENTS/CLAUDE、ADR、Design Tokens              | Rust/Swift/TS 空项目构建；Bridge fixture 往返                              |
| S1         | Rust Core + Swift Bridge | agentd、DbActor、Event Bus、SMAppService、PlatformBridge、Keychain、Migration、Heartbeat | 登录后自动运行；Bridge crash 可恢复；SQLite 不丢                           |
| S2         | 核心采集                 | Activity/System/Screenshot、隐私规则、Outbox                                             | 断网 2 小时不丢；权限撤销 5 秒内停止                                       |
| S3         | Cloud 与同步             | Better Auth、Pairing、Hono、Postgres、R2、Batch Sync                                     | 重复上传幂等；对象上传完整                                                 |
| S4         | Dashboard 核心           | Overview、Timeline、Screenshots、Activity、Devices/Commands                              | 远程截图 E2E；10k Timeline 虚拟化                                          |
| S5         | 扩展数据源与 WeChat      | Browser/File/Location、Rust WeChatProvider、4.1.12 Compatibility Spike、silent lifecycle | WeChat 未登录全程无 UI；登录后自动 ACTIVE 或可解释 unsupported；不重启微信 |
| S6         | 隐私、更新与 Beta        | Retention、Delete/Export、Sparkle、Rust/Bridge Migration Recovery、Sentry、安全审计      | 签名安装升级；删除不可复活；Beta 验收                                      |

## 26.5 核心风险

| **风险**         | **影响**              | **缓解**                                        |
|------------------|-----------------------|-------------------------------------------------|
| 微信版本变化     | Adapter 失效          | 实验性、版本探测、可替换 Provider、核心不依赖。 |
| 隐私信任不足     | 用户拒绝安装          | 默认关闭敏感 Collector、透明权限、排除和删除。  |
| 截图存储成本     | R2 成本和上传压力     | 压缩、去重、保留期、Wi-Fi 策略、local-only。    |
| macOS TCC/签名   | 权限丢失              | 稳定 Bundle ID/签名、Repair UI、更新测试。      |
| 后台被系统关闭   | 数据中断              | SMAppService 状态、Heartbeat、Dashboard 告警。  |
| 事件量膨胀       | DB/查询成本           | Session 聚合、批量、保留、分区与投影。          |
| 文档/Schema 漂移 | Code Agent 实现不一致 | 数据字典、枚举、OpenAPI 和 CI 集合检查。        |

# 27. V0 产品与技术验收

## 27.1 产品验收场景

| **场景** | **通过标准**                                                      |
|----------|-------------------------------------------------------------------|
| 首次安装 | 用户完成配对、后台项和 Activity 权限，5 分钟内 Web 出现首条事件。 |
| 持续采集 | 连续 24 小时无崩溃；睡眠/唤醒后恢复。                             |
| 离线恢复 | 断网 2 小时后恢复，事件按时间顺序同步，无重复。                   |
| 远程截图 | 在线设备从 Web 下发，15 秒内显示截图；缺权限给明确错误。          |
| 隐私暂停 | Web/本地暂停后 5 秒内停止新敏感事件；恢复后不补造暂停期间数据。   |
| 删除     | 删除截图和消息后 Dashboard、搜索、对象存储清理，离线设备不复活。  |
| WeChat   | 支持版本可增量；不支持版本显示 unsupported，不影响 Agent。        |
| 更新     | 从上一 Beta 检查、下载、备份、安装、Migration、重启；失败可恢复。 |

## 27.2 技术验收

| **领域** | **验收条件**                                                       |
|----------|--------------------------------------------------------------------|
| 安全     | Keychain、权限门禁、签名更新、无凭据日志、Workspace Scope 测试。   |
| 稳定     | Agent/Adapter crash 可恢复，Outbox 不丢，子进程超时可终止。        |
| 数据     | Cloud/Local Migration 从 0 和历史版本通过；Event/Projection 一致。 |
| 性能     | 满足 §25.3；Agent 空闲低资源，Timeline 流畅。                      |
| 可观测   | 每个错误有 code、requestId/commandId、可导出诊断。                 |
| 可替换   | WeChat、Storage、Map、Browser 均通过 Contract/Adapter。            |
| 文档     | AGENTS、CLAUDE、Data Dictionary、OpenAPI、ADR 与代码同步。         |

**PART VI 第六部分：附录**

保存术语、最终决策、枚举、错误码、字段级数据字典、合同模板和工程检查单。

# 附录 A：术语表

| **术语**            | **定义**                                                                  |
|---------------------|---------------------------------------------------------------------------|
| Agent Service       | 用户登录会话中常驻的 macOS 后台进程，负责 Collector、SQLite、同步和命令。 |
| Setup/Repair App    | 仅首次安装、权限修复、更新/数据库故障时出现的最小本地 UI。                |
| Collector           | 从单一系统或应用数据源采集并生成标准 Event 的独立模块。                   |
| Adapter             | 把外部工具、CLI、Provider 或应用数据映射到稳定合同的实现。                |
| Event               | 不可变的时间点事实；所有业务数据的共同语言。                              |
| Projection          | 从 Event 派生的查询模型，例如 ActivitySession、Message、Screenshot。      |
| Outbox              | 本地事务内创建的待同步记录，保证网络失败不丢数据。                        |
| Attachment          | 截图、诊断包等二进制；通过 Object Storage 上传。                          |
| Permission Snapshot | 某设备某时刻各 TCC/后台权限的状态记录。                                   |
| Device Pairing      | Web 用户把本地 Agent 与账户关联的一次性授权流程。                         |
| Command             | Web 向特定设备下发的有限远程任务，不等于远程桌面。                        |
| Tombstone           | 同步删除标记，防止离线设备恢复已删除数据。                                |

# 附录 B：最终决策清单

| **问题**                        | **最终答案**                                                                                                 |
|---------------------------------|--------------------------------------------------------------------------------------------------------------|
| 主产品是 Desktop 还是 Web？     | Web Dashboard。macOS 只有 Headless Agent 与按需 Setup/Repair UI。                                            |
| Agent 使用什么语言？            | Rust Core Runtime；Swift 只作为 macOS Platform Bridge 和 Setup/Updater。                                     |
| Rust 与 Swift 如何通信？        | 0600 Unix Domain Socket + 版本化 length-prefixed MessagePack/JSON；禁止共享可变业务状态。                    |
| 本地与云端关系？                | 本地持久队列 + 实时云同步；云端是跨设备查询事实源。                                                          |
| WeChat 如何初始化？             | 授权后静默等待；优先验证 Keychain Key，缺失时扫描当前已登录进程；未登录不处理。                              |
| 是否会退出/打开微信或提示登录？ | 正常后台流程不会。LLDB Active Extraction 仅显式维修/开发模式。                                               |
| 是否自动关闭 SIP 或重签微信？   | 禁止。能力不满足时静默等待或在主动诊断页说明。                                                               |
| WeChat 实时机制？               | session/WAL 变化检测 + per-talker sort_seq Cursor + message shard 增量查询，不使用 unread/summary 代替消息。 |
| 数据是否必须先写本地？          | 是。Event/Outbox 同事务；Collector 不等待网络。                                                              |
| 能否后台静默更新？              | 同权限范围可后台下载并在安全点安装；新增权限必须用户参与。                                                   |
| 是否读取 Cookie/密码？          | 禁止。                                                                                                       |
| 是否做远程控制？                | V0 不做；RustDesk/Computer Use 属于后续 Action Layer。                                                       |
| 是否做 AI？                     | V0 不做，但 Event/Projection 为未来 Memory 预留。                                                            |

# 附录 C：核心枚举与状态字典

| **枚举**                | **值**                                                                                                                                            | **说明**                  |
|-------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------|
| agent_status            | unpaired, initializing, waiting_permission, running, degraded, sleeping, updating, repair, stopped                                                | Agent 主状态              |
| bridge_status           | disconnected, handshaking, ready, degraded, incompatible, stopped                                                                                 | Rust/Swift Bridge         |
| collector_status        | disabled, permission_required, initializing, running, paused, degraded, unsupported, error                                                        | Collector 状态            |
| permission_status       | not_determined, granted, denied, restricted, unavailable                                                                                          | 系统权限                  |
| event_sync_state        | pending, sending, acked, conflict, dead_letter                                                                                                    | 本地事件同步              |
| command_status          | queued, dispatched, running, waiting_permission, succeeded, failed, cancelled, expired                                                            | 远程命令                  |
| command_type            | capture_screenshot, pause_collection, resume_collection, refresh_health, check_update, export_diagnostics                                         | V0 命令白名单             |
| provider_status         | disabled, waiting_source, checking_stored_key, passive_scanning, verifying_database, active, degraded, capability_unavailable, unsupported, error | 通信 Provider             |
| wechat_key_type         | raw_key, enc_key_pair_set                                                                                                                         | Keychain KeyMaterial 类型 |
| communication_sync_mode | full, metadata_only, local_only                                                                                                                   | 通信同步策略              |
| screenshot_trigger      | timer, app_switch_stable, manual_local, remote_command, communication_event                                                                       | 截图触发                  |
| file_action             | created, updated, deleted, renamed                                                                                                                | 文件事件                  |
| device_presence         | online, stale, offline, sleeping, revoked                                                                                                         | 设备状态                  |
| update_channel          | stable, beta, internal                                                                                                                            | 更新通道                  |
| update_status           | idle, checking, available, downloading, downloaded, deferred, installing, succeeded, failed                                                       | 更新状态                  |
| sensitivity             | public, normal, medium, high, secret                                                                                                              | 数据敏感级别              |

# 附录 D：核心错误码

| **类别**    | **错误码**                                                                                                                                                                                                 |
|-------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 认证/设备   | AUTH_REQUIRED, DEVICE_UNPAIRED, DEVICE_REVOKED, DEVICE_TOKEN_EXPIRED                                                                                                                                       |
| 权限/Bridge | PERMISSION_REQUIRED, PERMISSION_DENIED, BACKGROUND_ITEM_DISABLED, BRIDGE_UNAVAILABLE, BRIDGE_PROTOCOL_INCOMPATIBLE, BRIDGE_TIMEOUT                                                                         |
| Collector   | COLLECTOR_INIT_FAILED, COLLECTOR_TIMEOUT, COLLECTOR_UNSUPPORTED, COLLECTOR_DEGRADED                                                                                                                        |
| 截图        | SCREEN_CAPTURE_FAILED, SCREEN_SOURCE_UNAVAILABLE, SCREEN_PRIVACY_BLOCKED                                                                                                                                   |
| 活动        | ACCESSIBILITY_UNAVAILABLE, WINDOW_TITLE_UNAVAILABLE                                                                                                                                                        |
| 浏览器      | BROWSER_EXTENSION_MISSING, NATIVE_HOST_UNAUTHORIZED, BROWSER_SCHEMA_UNSUPPORTED                                                                                                                            |
| 微信        | WECHAT_WAITING_SOURCE, WECHAT_CAPABILITY_UNAVAILABLE, WECHAT_VERSION_UNSUPPORTED, WECHAT_PASSIVE_SCAN_FAILED, WECHAT_KEY_INVALID, WECHAT_DATABASE_UNREADABLE, WECHAT_SCHEMA_UNSUPPORTED, WECHAT_CURSOR_GAP |
| 文件/位置   | FILE_SCOPE_INVALID, FSEVENTS_DROPPED, LOCATION_PERMISSION_REQUIRED, LOCATION_UNAVAILABLE                                                                                                                   |
| 同步        | SYNC_OFFLINE, SYNC_RATE_LIMITED, SYNC_SCHEMA_UNSUPPORTED, SYNC_PAYLOAD_REJECTED, SYNC_DEAD_LETTER                                                                                                          |
| 对象        | OBJECT_UPLOAD_FAILED, OBJECT_HASH_MISMATCH, OBJECT_NOT_FOUND                                                                                                                                               |
| 命令        | COMMAND_EXPIRED, COMMAND_CANCELLED, COMMAND_PERMISSION_REQUIRED, COMMAND_DUPLICATE                                                                                                                         |
| 更新        | UPDATE_CHECK_FAILED, UPDATE_SIGNATURE_INVALID, UPDATE_TASKS_BUSY, UPDATE_BRIDGE_INCOMPATIBLE, UPDATE_MIGRATION_FAILED, UPDATE_REPAIR_REQUIRED                                                              |
| 数据库      | DB_OPEN_FAILED, DB_INTEGRITY_FAILED, DB_MIGRATION_FAILED, DB_VERSION_TOO_NEW, DISK_SPACE_LOW                                                                                                               |

# 附录 E：数据字典

## E.1 公共字段模板

| **模板**         | **字段集合**                                                                                                        | **语义**             |
|------------------|---------------------------------------------------------------------------------------------------------------------|----------------------|
| CLOUD_ROOT       | id, created_at, updated_at, deleted_at, revision                                                                    | 可更新云端根对象。   |
| WORKSPACE_ENTITY | id, workspace_id, created_at, updated_at, deleted_at, revision                                                      | Workspace 业务对象。 |
| APPEND_EVENT     | id, workspace_id, device_id, occurred_at, created_at                                                                | 创建即锁定的事实。   |
| EXECUTION        | id, workspace_id, device_id, status, started_at, finished_at, error_code, error_detail, idempotency_key, created_at | 命令/同步/更新执行。 |
| LOCAL_ROW        | id TEXT, created_at_ms, updated_at_ms                                                                               | SQLite 本地对象。    |

**devices · 公共字段：CLOUD_ROOT**

| **字段**          | **类型**    | **约束/默认**     | **说明**                              |
|-------------------|-------------|-------------------|---------------------------------------|
| workspace_id      | uuid        | NOT NULL FK       | 所属 Workspace                        |
| user_id           | uuid        | NOT NULL FK       | 设备所有者                            |
| device_key        | text        | UNIQUE NOT NULL   | 安装生成的匿名稳定标识                |
| display_name      | text        | NOT NULL          | 用户可改设备名                        |
| platform          | text        | NOT NULL          | macos                                 |
| arch              | text        | NOT NULL          | arm64/x86_64                          |
| os_version        | text        | NOT NULL          | 系统版本                              |
| agent_version     | text        | NOT NULL          | 最近 Agent 版本                       |
| last_heartbeat_at | timestamptz | NULL              | 最近心跳                              |
| presence          | text        | DEFAULT 'offline' | online/stale/offline/sleeping/revoked |
| revoked_at        | timestamptz | NULL              | 撤销时间                              |
| public_key        | text        | NULL              | 设备公钥                              |

**collector_instances · 公共字段：WORKSPACE_ENTITY**

| **字段**                | **类型**    | **约束/默认** | **说明**         |
|-------------------------|-------------|---------------|------------------|
| device_id               | uuid        | NOT NULL FK   | 设备             |
| collector_key           | text        | NOT NULL      | Collector 稳定键 |
| collector_version       | text        | NOT NULL      | 运行版本         |
| status                  | text        | NOT NULL      | collector_status |
| desired_config_revision | bigint      | DEFAULT 0     | 云端期望配置版本 |
| applied_config_revision | bigint      | DEFAULT 0     | 设备已应用版本   |
| last_event_at           | timestamptz | NULL          | 最后事件         |
| last_health_at          | timestamptz | NULL          | 健康检查         |
| last_error_code         | text        | NULL          | 最近错误         |

**permission_snapshots · 公共字段：APPEND_EVENT**

| **字段**       | **类型** | **约束/默认** | **说明**                         |
|----------------|----------|---------------|----------------------------------|
| collector_key  | text     | NULL          | 关联 Collector，可为空           |
| permission_key | text     | NOT NULL      | screen/accessibility/location 等 |
| status         | text     | NOT NULL      | permission_status                |
| source         | text     | NOT NULL      | system_check/user_action         |

**events · 公共字段：APPEND_EVENT**

| **字段**       | **类型**    | **约束/默认**    | **说明**                                   |
|----------------|-------------|------------------|--------------------------------------------|
| event_id       | uuid        | NOT NULL         | 设备生成 Event ID；device_id+event_id 唯一 |
| event_type     | text        | NOT NULL         | Domain.Action                              |
| source         | text        | NOT NULL         | Collector/Adapter key                      |
| schema_version | integer     | NOT NULL         | Payload 版本                               |
| payload_json   | jsonb       | DEFAULT '{}'     | 标准 payload；secret 禁止                  |
| sensitivity    | text        | DEFAULT 'normal' | 敏感级别                                   |
| object_file_id | uuid        | NULL FK          | 关联二进制                                 |
| ingested_at    | timestamptz | NOT NULL         | 云端提交时间                               |

**activity_sessions · 公共字段：WORKSPACE_ENTITY**

| **字段**         | **类型**    | **约束/默认** | **说明**                    |
|------------------|-------------|---------------|-----------------------------|
| device_id        | uuid        | NOT NULL FK   | 设备                        |
| source_event_id  | uuid        | NULL FK       | 来源事件                    |
| bundle_id        | text        | NOT NULL      | 应用 Bundle ID              |
| application_name | text        | NOT NULL      | 应用名                      |
| window_title     | text        | NULL          | 最后窗口标题                |
| started_at       | timestamptz | NOT NULL      | 开始                        |
| ended_at         | timestamptz | NOT NULL      | 结束                        |
| duration_seconds | integer     | NOT NULL      | 时长                        |
| activity_state   | text        | NOT NULL      | active/idle/locked/sleeping |
| recovered        | boolean     | DEFAULT false | 崩溃恢复截断                |

**object_files · 公共字段：WORKSPACE_ENTITY**

| **字段**                | **类型**    | **约束/默认**      | **说明**                   |
|-------------------------|-------------|--------------------|----------------------------|
| bucket                  | text        | NOT NULL           | R2 bucket                  |
| object_key              | text        | UNIQUE NOT NULL    | 对象键                     |
| mime_type               | text        | NOT NULL           | MIME                       |
| size_bytes              | bigint      | NOT NULL           | 大小                       |
| sha256                  | text        | NOT NULL           | 哈希                       |
| retention_class         | text        | DEFAULT 'standard' | temporary/standard/archive |
| deleted_from_storage_at | timestamptz | NULL               | 物理删除时间               |

**screenshots · 公共字段：WORKSPACE_ENTITY**

| **字段**         | **类型**    | **约束/默认** | **说明**             |
|------------------|-------------|---------------|----------------------|
| device_id        | uuid        | NOT NULL FK   | 设备                 |
| event_id         | uuid        | NOT NULL FK   | screen.created Event |
| object_file_id   | uuid        | NOT NULL FK   | 图片对象             |
| capture_group_id | uuid        | NOT NULL      | 多屏同次捕获         |
| display_id       | text        | NOT NULL      | 显示器标识           |
| captured_at      | timestamptz | NOT NULL      | 捕获时间             |
| trigger          | text        | NOT NULL      | screenshot_trigger   |
| bundle_id        | text        | NULL          | 前台应用             |
| window_title     | text        | NULL          | 窗口标题             |
| width            | integer     | NOT NULL      | 宽                   |
| height           | integer     | NOT NULL      | 高                   |

**browser_visits · 公共字段：WORKSPACE_ENTITY**

| **字段**         | **类型**    | **约束/默认** | **说明**          |
|------------------|-------------|---------------|-------------------|
| device_id        | uuid        | NOT NULL FK   | 设备              |
| browser          | text        | NOT NULL      | chrome/edge/brave |
| profile_key      | text        | NULL          | 本地 profile hash |
| url              | text        | NOT NULL      | URL               |
| title            | text        | NULL          | 页面标题          |
| domain           | text        | NOT NULL      | 归一化域名        |
| started_at       | timestamptz | NOT NULL      | 开始              |
| ended_at         | timestamptz | NOT NULL      | 结束              |
| duration_seconds | integer     | NOT NULL      | 活跃秒数          |

**conversations · 公共字段：WORKSPACE_ENTITY**

| **字段**                 | **类型**    | **约束/默认** | **说明**                      |
|--------------------------|-------------|---------------|-------------------------------|
| device_id                | uuid        | NOT NULL FK   | 来源设备                      |
| adapter_key              | text        | NOT NULL      | wechat 等                     |
| external_conversation_id | text        | NOT NULL      | Provider ID/hash              |
| display_name             | text        | NULL          | 会话名                        |
| conversation_type        | text        | NOT NULL      | direct/group/system           |
| sync_mode                | text        | NOT NULL      | full/metadata_only/local_only |
| last_message_at          | timestamptz | NULL          | 最后消息                      |

**messages · 公共字段：WORKSPACE_ENTITY**

| **字段**            | **类型**    | **约束/默认**  | **说明**                  |
|---------------------|-------------|----------------|---------------------------|
| conversation_id     | uuid        | NOT NULL FK    | 会话                      |
| adapter_key         | text        | NOT NULL       | Provider                  |
| external_message_id | text        | NOT NULL       | Provider 消息 ID          |
| sender_id           | text        | NULL           | 外部 sender 标识          |
| sender_name         | text        | NULL           | 显示名                    |
| message_type        | text        | NOT NULL       | text/image/file/...       |
| body                | text        | NULL           | full 模式正文             |
| body_length         | integer     | NULL           | metadata 模式长度         |
| sent_at             | timestamptz | NOT NULL       | 消息时间                  |
| direction           | text        | NULL           | incoming/outgoing/unknown |
| attachment_json     | jsonb       | DEFAULT '\[\]' | 附件 metadata             |

**location_points · 公共字段：APPEND_EVENT**

| **字段**            | **类型**         | **约束/默认** | **说明**     |
|---------------------|------------------|---------------|--------------|
| latitude            | double precision | NOT NULL      | WGS84 纬度   |
| longitude           | double precision | NOT NULL      | 经度         |
| horizontal_accuracy | double precision | NOT NULL      | 米           |
| altitude            | double precision | NULL          | 海拔         |
| reduced_accuracy    | boolean          | DEFAULT false | 是否模糊精度 |
| geohash             | text             | NULL          | 区域聚合     |

**file_events · 公共字段：APPEND_EVENT**

| **字段**     | **类型** | **约束/默认** | **说明**             |
|--------------|----------|---------------|----------------------|
| scope_id     | uuid     | NOT NULL      | 授权目录 Scope       |
| action       | text     | NOT NULL      | file_action          |
| path_display | text     | NULL          | 按隐私策略的相对路径 |
| path_hash    | text     | NOT NULL      | 稳定去重             |
| file_name    | text     | NULL          | 文件名               |
| extension    | text     | NULL          | 扩展名               |
| size_bytes   | bigint   | NULL          | 大小                 |

**agent_commands · 公共字段：WORKSPACE_ENTITY**

| **字段**           | **类型**    | **约束/默认**    | **说明**        |
|--------------------|-------------|------------------|-----------------|
| device_id          | uuid        | NOT NULL FK      | 目标设备        |
| command_type       | text        | NOT NULL         | 白名单类型      |
| parameters_json    | jsonb       | DEFAULT '{}'     | Schema 校验参数 |
| status             | text        | DEFAULT 'queued' | command_status  |
| expires_at         | timestamptz | NOT NULL         | TTL             |
| idempotency_key    | text        | UNIQUE NOT NULL  | 幂等            |
| created_by_user_id | uuid        | NOT NULL FK      | 操作者          |
| result_json        | jsonb       | DEFAULT '{}'     | 最终结果引用    |

**agent_command_attempts · 公共字段：EXECUTION**

| **字段**   | **类型**     | **约束/默认** | **说明** |
|------------|--------------|---------------|----------|
| command_id | uuid         | NOT NULL FK   | 命令     |
| attempt_no | integer      | NOT NULL      | 尝试序号 |
| progress   | numeric(5,2) | NULL          | 进度     |
| checkpoint | text         | NULL          | 恢复点   |
| result_ref | text         | NULL          | 结果引用 |

**agent_releases · 公共字段：CLOUD_ROOT**

| **字段**                  | **类型** | **约束/默认**   | **说明**                  |
|---------------------------|----------|-----------------|---------------------------|
| version                   | text     | UNIQUE NOT NULL | 语义版本                  |
| channel                   | text     | NOT NULL        | stable/beta/internal      |
| status                    | text     | DEFAULT 'draft' | draft/published/suspended |
| minimum_supported_version | text     | NULL            | 最低版本                  |
| min_db_schema_version     | integer  | NOT NULL        | 支持 DB 下限              |
| max_db_schema_version     | integer  | NOT NULL        | 支持 DB 上限              |
| target_db_schema_version  | integer  | NOT NULL        | 目标 DB                   |
| release_notes             | text     | NULL            | 版本说明                  |

**retention_policies · 公共字段：WORKSPACE_ENTITY**

| **字段**             | **类型** | **约束/默认** | **说明**                                |
|----------------------|----------|---------------|-----------------------------------------|
| data_type            | text     | NOT NULL      | events/screenshots/messages/location 等 |
| retention_days       | integer  | NOT NULL      | 保留天数                                |
| local_retention_days | integer  | NULL          | 设备本地保留                            |
| enabled              | boolean  | DEFAULT true  | 是否启用                                |

## E.2 Local SQLite 字段字典

**events_local · 公共字段：LOCAL_ROW**

| **字段**        | **类型** | **约束/默认**         | **说明**      |
|-----------------|----------|-----------------------|---------------|
| event_id        | TEXT     | UNIQUE NOT NULL       | UUIDv7        |
| device_id       | TEXT     | NOT NULL              | 设备          |
| event_type      | TEXT     | NOT NULL              | Event type    |
| source          | TEXT     | NOT NULL              | Collector key |
| schema_version  | INTEGER  | NOT NULL              | Payload 版本  |
| occurred_at_ms  | INTEGER  | NOT NULL              | UTC epoch ms  |
| payload_json    | TEXT     | NOT NULL DEFAULT '{}' | JSON          |
| object_local_id | TEXT     | NULL                  | 本地附件      |

**sync_outbox · 公共字段：LOCAL_ROW**

| **字段**           | **类型** | **约束/默认**     | **说明**                                   |
|--------------------|----------|-------------------|--------------------------------------------|
| entity_type        | TEXT     | NOT NULL          | event/object/command_result                |
| entity_id          | TEXT     | NOT NULL          | 本地对象 ID                                |
| priority           | INTEGER  | DEFAULT 50        | 越小越高                                   |
| status             | TEXT     | DEFAULT 'pending' | pending/sending/acked/conflict/dead_letter |
| attempt_count      | INTEGER  | DEFAULT 0         | 尝试次数                                   |
| next_attempt_at_ms | INTEGER  | NULL              | 下次时间                                   |
| batch_id           | TEXT     | NULL              | 发送批次                                   |
| last_error_code    | TEXT     | NULL              | 错误码                                     |

**screenshots_local · 公共字段：LOCAL_ROW**

| **字段**         | **类型** | **约束/默认**     | **说明**        |
|------------------|----------|-------------------|-----------------|
| screenshot_id    | TEXT     | UNIQUE NOT NULL   | 截图 ID         |
| event_id         | TEXT     | NOT NULL          | 关联 Event      |
| local_path       | TEXT     | NOT NULL          | 应用目录路径    |
| content_hash     | TEXT     | NOT NULL          | SHA256          |
| size_bytes       | INTEGER  | NOT NULL          | 大小            |
| upload_status    | TEXT     | DEFAULT 'pending' | 上传状态        |
| remote_object_id | TEXT     | NULL              | 云端 ObjectFile |

**collector_states · 公共字段：LOCAL_ROW**

| **字段**         | **类型** | **约束/默认**   | **说明**                    |
|------------------|----------|-----------------|-----------------------------|
| collector_key    | TEXT     | PRIMARY KEY     | Collector                   |
| status           | TEXT     | NOT NULL        | collector_status            |
| version          | TEXT     | NOT NULL        | 版本                        |
| desired_revision | INTEGER  | DEFAULT 0       | 期望配置                    |
| applied_revision | INTEGER  | DEFAULT 0       | 已应用                      |
| last_event_at_ms | INTEGER  | NULL            | 最后成功 Event，UTC epoch ms |
| last_health_at_ms | INTEGER | NULL            | 最后健康观测，UTC epoch ms  |
| last_error_code  | TEXT     | NULL            | 错误                        |
| created_at_ms    | INTEGER  | NOT NULL        | 创建时间，UTC epoch ms      |
| updated_at_ms    | INTEGER  | NOT NULL        | 更新时间，UTC epoch ms      |

`collector_key` 主键已覆盖状态读取，本切片不增加额外索引。每个 Collector 结果的 Event、对应 Sync Outbox 行和最新 `collector_states` 必须在 DbActor 的同一 SQLite 事务中提交；任一写入失败则全部回滚。

**agent_commands_local · 公共字段：LOCAL_ROW**

| **字段**        | **类型** | **约束/默认**   | **说明**    |
|-----------------|----------|-----------------|-------------|
| command_id      | TEXT     | UNIQUE NOT NULL | 云端命令 ID |
| command_type    | TEXT     | NOT NULL        | 类型        |
| parameters_json | TEXT     | DEFAULT '{}'    | 参数        |
| status          | TEXT     | NOT NULL        | 状态        |
| expires_at_ms   | INTEGER  | NOT NULL        | TTL         |
| checkpoint      | TEXT     | NULL            | 恢复点      |

**schema_migrations · 公共字段：—**

| **字段**        | **类型** | **约束/默认** | **说明**                 |
|-----------------|----------|---------------|--------------------------|
| migration_id    | TEXT     | PRIMARY KEY   | 不可变 ID                |
| checksum        | TEXT     | NOT NULL      | 脚本哈希                 |
| app_version     | TEXT     | NOT NULL      | 发布版本                 |
| started_at_ms   | INTEGER  | NOT NULL      | 开始                     |
| completed_at_ms | INTEGER  | NULL          | 完成                     |
| status          | TEXT     | NOT NULL      | running/completed/failed |

# 附录 F：Collector / Adapter 合同

## F.1 Collector Event Factory

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th>protocol EventFactory {<br />
func makeEvent&lt;T: Encodable&gt;(<br />
type: EventType,<br />
source: String,<br />
occurredAt: Date,<br />
payload: T,<br />
sensitivity: Sensitivity,<br />
attachment: LocalAttachment?<br />
) async throws -&gt; LocalEvent<br />
}</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## F.2 WeChat Provider DTO

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><strong>Rust WeChat Provider DTO</strong><br />
<strong>#[derive(Clone, Serialize, Deserialize)]</strong><br />
<strong>pub struct WechatMessageDto {</strong><br />
<strong>pub external_message_id: String,</strong><br />
<strong>pub external_conversation_id: String,</strong><br />
<strong>pub sender_id: Option&lt;String&gt;,</strong><br />
<strong>pub sender_name: Option&lt;String&gt;,</strong><br />
<strong>pub message_type: String,</strong><br />
<strong>pub body: Option&lt;String&gt;,</strong><br />
<strong>pub sent_at_ms: i64,</strong><br />
<strong>pub direction: Option&lt;String&gt;,</strong><br />
<strong>pub sort_seq: i64,</strong><br />
<strong>pub server_id: Option&lt;i64&gt;,</strong><br />
<strong>pub attachments: Vec&lt;WechatAttachmentDto&gt;,</strong><br />
<strong>}</strong><br />
<br />
<strong>#[async_trait]</strong><br />
<strong>pub trait WechatProvider: CommunicationProvider {</strong><br />
<strong>async fn verify_stored_key(&amp;self) -&gt; Result&lt;KeyVerification&gt;;</strong><br />
<strong>async fn passive_acquire_key(&amp;self) -&gt; Result&lt;KeyMaterial&gt;;</strong><br />
<strong>async fn fetch_talker_after(&amp;self, talker: &amp;str, sort_seq: i64) -&gt; Result&lt;MessageBatch&gt;;</strong><br />
<strong>}</strong></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

## F.3 Adapter 强制能力

| **能力**              | **要求**                           |
|-----------------------|------------------------------------|
| timeout               | 每个子进程/请求有硬超时。          |
| retry classification  | 明确 retryable/non-retryable。     |
| version probe         | Provider/源应用/系统版本可查询。   |
| schema version        | 输入输出版本化。                   |
| redacted logging      | 不记录 Key、正文和路径默认。       |
| fixture contract test | 成功/空/错误/新字段/未知类型样例。 |
| health                | 状态、延迟、最后成功、错误码。     |

# 附录 G：AGENTS.md 模板

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th># Personal Computer Agent Engineering Rules<br />
<br />
## Facts and scope<br />
- Read `/docs/PRODUCT_TECH_SPEC.docx` and `ARCHITECTURE.md` before changing architecture.<br />
- V0 is Web-dashboard-first with a Rust Core and Swift macOS Bridge. Do not add Electron UI, AI, keylogging, camera, microphone, message sending, or remote control.<br />
<br />
## Architecture<br />
- Rust Collectors produce Events only. They must not call cloud APIs.<br />
- Swift PlatformBridge exposes Apple capabilities only; it must not access SQLite, Cloud APIs, retention, sync or Provider state.<br />
- WeChat normal flow must never kill/open/re-sign WeChat or prompt login. It may silently wait and passively scan an already logged-in process.<br />
- Web UI never imports database or provider SDKs.<br />
- Cloud business code depends on Ports; SDKs live in infrastructure adapters.<br />
- Secrets live in Keychain/server secret store, never SQLite, payloads, or logs.<br />
<br />
## Database<br />
- Every schema change requires migration, data dictionary, index review, sync impact and tests.<br />
- Published migrations are immutable; fix with a new migration.<br />
- Event facts are append-only.<br />
<br />
## Implementation discipline<br />
- State assumptions before coding.<br />
- Make surgical changes; no unrelated refactors.<br />
- Define success criteria and run verification before completion.<br />
- Do not claim tests passed unless commands and results are visible.<br />
<br />
## Required verification<br />
- format<br />
- lint<br />
- typecheck / Swift build<br />
- unit + contract tests<br />
- migration from empty and previous version<br />
- dependency boundary<br />
- build / package smoke test</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

# 附录 H：CLAUDE.md 模板

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th># Claude Code Instructions<br />
<br />
@AGENTS.md<br />
<br />
## Working method<br />
1. Read the relevant spec section and local AGENTS.md.<br />
2. Restate scope, assumptions and acceptance criteria.<br />
3. Inspect existing code before proposing new abstractions.<br />
4. Implement the smallest change that satisfies the contract.<br />
5. Run required verification; report exact commands and failures.<br />
<br />
## Prohibited<br />
- Do not create a second Event model, Sync engine, permission enum or error-code namespace.<br />
- Do not bypass TCC or add hidden collection.<br />
- Do not install new dependencies without explaining why existing packages are insufficient.<br />
- Do not modify published migrations.</th>
</tr>
</thead>
<tbody>
</tbody>
</table>

# 附录 I：Migration / Pull Request 检查单

| **检查项** | **通过标准**                                                                 |
|------------|------------------------------------------------------------------------------|
| 需求与边界 | Issue 有可测试验收条件；不额外扩展 AI/远控/移动端。                          |
| 架构       | 遵守 Collector→Event→Store→Sync；新增抽象有必要；高成本决策有 ADR。          |
| 数据库     | Schema、Migration、Backfill、字典、索引、Cloud/Local/Sync 影响齐全。         |
| API        | OpenAPI/Bridge Schema 更新、向后兼容、错误码和 Rust/Swift Contract Fixture。 |
| 测试       | Unit、Integration、Contract、E2E、失败路径。                                 |
| 性能       | Agent CPU/内存、查询、批次、附件影响已评估。                                 |
| 安全       | 权限、Keychain、日志、对象 URL、子进程、删除已审查。                         |
| 隐私       | 采集目的、默认开关、排除和保留更新。                                         |
| 文档       | AGENTS、ADR、Data Dictionary、UI 清单同步。                                  |
| 验证证据   | CI/本地命令结果可见；未运行项明确。                                          |

# 附录 J：UI 页面、抽屉与弹窗清单

| **模块**      | **页面/抽屉/弹窗**                                                                                                   |
|---------------|----------------------------------------------------------------------------------------------------------------------|
| 全局          | Workspace/Device/Date Scope、Search、Task/Alert Center、Pause Banner                                                 |
| Overview      | Device Status、Today Activity、Collectors、Recent Timeline、Screenshot Preview                                       |
| Timeline      | Filter Bar、Virtual List、Event Detail Drawer、Raw Payload Developer Panel                                           |
| Screenshots   | Grid、Preview Dialog、Bulk Delete、Export Dialog、Retention Notice                                                   |
| Activity      | Summary、App Ranking、Session Table、Category Editor                                                                 |
| Communication | Provider Health、Conversation List、Message Viewer、Sync Mode、主动诊断页；正常 waiting_source/passive scan 无弹窗。 |
| Browser       | Visit List、Domain Stats、Extension Setup、Excluded Domain Editor                                                    |
| Files         | Directory Scopes、Event List、Ignore Rules、Missing Scope Warning                                                    |
| Location      | Map、Point List、Accuracy Explanation、Enable/Disable Confirmation                                                   |
| Devices       | List、Detail、Permissions、Collectors、Commands、Revoke Dialog、Diagnostics                                          |
| Settings      | Collector Config、Privacy Schedule、Retention、Export/Delete、Update、Account                                        |
| Local Setup   | Pairing、Login Item、Permission Request、Repair、Database Recovery、Update Failure                                   |

# 附录 K：ADR 索引与技术参考

## K.1 建议 ADR

| **ADR** | **主题**                                                                     | **状态**                   |
|---------|------------------------------------------------------------------------------|----------------------------|
| ADR-001 | Web Dashboard 为主产品，macOS 仅 Headless Agent                              | Accepted                   |
| ADR-002 | Rust Core Runtime + Swift macOS Bridge，替代 Swift-only                      | Accepted · Supersedes V1.0 |
| ADR-003 | Event-driven + Local Outbox                                                  | Accepted                   |
| ADR-004 | WeChat 为可替换 Rust Provider，不拖垮 Agent Core                             | Accepted                   |
| ADR-005 | WeChat 授权后静默 waiting_source / passive scan；自动 Active Extraction 禁止 | Accepted                   |
| ADR-006 | Sparkle 更新完整 App Bundle，并协调 Rust/Bridge/SQLite                       | Accepted                   |
| ADR-007 | Chromium Extension + Rust Native Messaging 获取 URL                          | Accepted                   |
| ADR-008 | V0 不做 AI、消息发送与远程控制                                               | Accepted                   |

## K.2 技术参考

| **编号** | **资料**                          | **链接**                                                                                                 |
|----------|-----------------------------------|----------------------------------------------------------------------------------------------------------|
| R1       | Apple SMAppService                | https://developer.apple.com/documentation/servicemanagement/smappservice                                 |
| R2       | Apple ScreenCaptureKit            | https://developer.apple.com/documentation/screencapturekit                                               |
| R3       | Apple Core Location Authorization | https://developer.apple.com/documentation/corelocation/requesting-authorization-to-use-location-services |
| R4       | Apple AXUIElement                 | https://developer.apple.com/documentation/applicationservices/axuielement                                |
| R5       | pandorafuture/wx-cli              | https://github.com/pandorafuture/wx-cli                                                                  |
| R6       | huohuoer/wechat-cli               | https://github.com/huohuoer/wechat-cli                                                                   |
| R7       | CipherTalk                        | https://github.com/ILoveBingLu/CipherTalk                                                                |
| R8       | Sparkle                           | https://github.com/sparkle-project/Sparkle                                                               |
| R9       | Rust Tokio                        | https://tokio.rs/                                                                                        |
| R10      | rusqlite                          | https://github.com/rusqlite/rusqlite                                                                     |

<table>
<colgroup>
<col style="width: 100%" />
</colgroup>
<thead>
<tr class="header">
<th><p><strong>文档结束</strong></p>
<p>本规格是 V0 的产品与技术主规格事实源。任何实现切片可以减少当期交付范围，但不得重新定义 Agent、Collector、Event、Sync、Permission、Retention、Command 和 Update 的语义。</p></th>
</tr>
</thead>
<tbody>
</tbody>
</table>

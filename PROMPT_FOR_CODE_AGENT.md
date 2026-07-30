# 可直接粘贴给 Code Agent 的启动指令

你正在开发 Personal Computer Agent V0。

请先完整阅读以下文件，并把它们视为事实源：

1. `00_START_HERE.md`
2. `docs/SOURCE_ERRATA.md`
3. `AGENTS.md`
4. `ARCHITECTURE.md`
5. `SECURITY.md`
6. `PERFORMANCE.md`
7. `docs/PRODUCT_TECH_SPEC_V1.1.md`
8. `tasks/S0_ENGINEERING_BASELINE.md`
9. `contracts/README.md`
10. `docs/adr/ADR-0001-rust-core-swift-bridge.md`
11. `docs/adr/ADR-0002-event-store-outbox.md`
12. `docs/adr/ADR-0003-silent-wechat-provider.md`
13. `docs/adr/ADR-0004-web-dashboard-first.md`

本轮只执行 S0，不要提前实现 S1-S6 的业务功能。

开始编码前先输出：

- 你理解的 S0 范围；
- 你发现的仓库现状；
- 需要成立的假设；
- 计划修改/新增的文件；
- S0 的可测试成功标准；
- 任何规格冲突或未决项。

执行规则：

- 只做满足 S0 退出门禁的最小完整改动。
- 不做无关重构。
- 不新增第二套 Event、错误码、状态枚举或 Bridge 协议。
- 不改变 Rust Core + Swift Bridge 架构。
- 不实现隐蔽采集或权限绕过。
- 所有验证必须真实执行并报告命令、退出码和关键输出。
- 未通过的验证必须明确列出，不得使用“应该可以”。

完成后按 `tasks/DEFINITION_OF_DONE.md` 和 `tasks/S0_ENGINEERING_BASELINE.md` 自检，并给出下一 Sprint 的阻塞项，但不要自行进入 S1。

# 先读这里

本仓库由 Personal Computer Agent Code Agent Development Pack 初始化。

## 第一条规则

**不要一次性实现整套系统。先执行 S0，只在 S0 退出门禁通过后进入 S1。**

## 必读顺序

1. `DEV_PACKAGE_MANIFEST.md`
2. `docs/SOURCE_ERRATA.md`
3. `AGENTS.md`
4. `ARCHITECTURE.md`
5. `SECURITY.md`
6. `PERFORMANCE.md`
7. `docs/PRODUCT_TECH_SPEC_V1.1.md`
8. 当前 Sprint 的 `tasks/Sx_*.md`
9. 与当前改动相关的 ADR 和 JSON Schema

## 当前第一任务

仅执行 `tasks/S0_ENGINEERING_BASELINE.md`。

S0 冻结 Monorepo、跨语言合同、错误码和状态枚举、依赖边界、CI、Migration 基线、本地开发命令、ADR 与文档门禁。S0 不实现真实 Collector、WeChat、云端部署或 Dashboard 功能页。

## 静默运行的准确含义

“静默”只表示用户已经在产品中显式启用该类采集后，数据源未就绪、Provider 等待、被动扫描与重试不反复弹窗、不打断用户。它不表示绕过授权、隐蔽安装或隐藏采集状态。

## 向 Code Agent 下达任务

使用根目录的 `PROMPT_FOR_CODE_AGENT.md`，并严格遵守当前 Sprint 退出门禁。

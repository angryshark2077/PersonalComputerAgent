# Personal Computer Agent · Code Agent Development Pack

版本：v1.0  
依据：`docs/PRODUCT_TECH_SPEC_V1.1.docx`  
主架构：Web-dashboard-first · Rust Core · Swift macOS Bridge · Event-driven · Privacy-by-design

## 事实源优先级

1. `docs/PRODUCT_TECH_SPEC_V1.1.docx`：产品语义、领域边界与终局架构事实源。
2. `docs/SOURCE_ERRATA.md`：源文档中已识别的明确残留矛盾及其解析规则。
3. `contracts/*.schema.json`：跨语言、跨进程和 API 的机器可读合同基线。
4. `ARCHITECTURE.md`、`SECURITY.md`、`PERFORMANCE.md`：面向实现的摘要事实源。
5. `tasks/S0_*.md` 至 `tasks/S6_*.md`：实施切片；不得重新定义主规格语义。
6. `repo-template/`：可复制的仓库骨架，不代表功能已经实现。

发生冲突时，必须停止实现并新增/更新 ADR；禁止 Code Agent 自行选择“看起来更合理”的一方。

## 开发包内容

- `00_START_HERE.md`：Code Agent 的阅读顺序与启动方法。
- `PROMPT_FOR_CODE_AGENT.md`：可直接粘贴给 Codex / Claude Code 的启动指令。
- `AGENTS.md`：仓库级强制工程规则。
- `CLAUDE.md`：Claude Code 增量规则。
- `ARCHITECTURE.md`：进程、模块、依赖、数据流和运行时不变量。
- `SECURITY.md`：权限、隐私、凭据、静默 Provider 和威胁门禁。
- `PERFORMANCE.md`：V0 性能预算与验证方式。
- `IMPLEMENTATION_PLAN.md`：七个两周 Sprint、依赖关系和里程碑。
- `ACCEPTANCE.md`：产品与技术验收矩阵。
- `contracts/`：Event、Bridge、Sync、Command、Provider、Error JSON Schema。
- `tasks/`：S0-S6 执行卡、DoD 和 Backlog。
- `docs/adr/`：冻结的关键架构决策。
- `docs/runbooks/`：WeChat 4.1.12 Spike、Migration Recovery、诊断包流程。
- `repo-template/`：Rust/Swift/TypeScript Monorepo 骨架。
- `prompts/`：实施、评审、调试和发布门禁提示词。
- `scripts/`：开发包校验与仓库初始化脚本。

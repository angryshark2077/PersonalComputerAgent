# Claude Code Instructions

@AGENTS.md

## Working method

1. 读取当前任务对应的主规格章节、当前 Sprint 卡和局部 AGENTS.md。
2. 重述范围、假设、不可变约束和验收条件。
3. 检查现有代码、Migration、Schema、Contract 和测试，避免建立第二套事实源。
4. 先提交实施计划，再执行最小完整改动。
5. 每个高风险阶段后运行局部验证，不把所有验证推迟到最后。
6. 完成前运行完整门禁，并报告真实命令与失败。
7. 发现规格矛盾时停止实现，登记 `docs/SOURCE_ERRATA.md` 或新增 ADR，不自行裁决。

## Prohibited

- 不得创建第二套 Event、Sync Engine、Permission Enum、Provider Status 或 Error Code namespace。
- 不得绕过 TCC、SIP、授权或增加隐蔽采集。
- 不得让正常 WeChat 流程退出、启动、重签或提示登录。
- 不得修改已发布 Migration。
- 不得在业务代码中直接导入 Provider SDK。
- 不得在没有证据时宣称“完成”。

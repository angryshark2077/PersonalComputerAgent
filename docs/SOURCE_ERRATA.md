# 源规格已识别矛盾

## ERRATA-001：§12.1 残留的 Swift-only 句子

`docs/PRODUCT_TECH_SPEC_V1.1.docx` 的标题、版本说明、§1.2、§1.3、§12.2-§12.7、§24.1 和 Sprint S1 均明确采用：

```text
Rust Core Runtime + Swift macOS Platform Bridge
```

但 §12.1 的“最终裁决”框中残留一句：

> V0 macOS Agent 使用 Swift 单语言实现。

该句与全文主架构冲突，属于重写时未清理的残留内容。

根据主规格 §1.4“正文中的最终决策高于附录；同一概念只保留一个权威定义”，并且 §1 是全文最高优先级决策摘要，本开发包的执行裁决固定为：

```text
Rust stable + Tokio 管理 Agent Runtime、Collector、Event、SQLite、Outbox、Sync、Command、Provider。
Swift 6.x 仅实现 macOS Platform Bridge、Setup/Repair、SMAppService 与 Sparkle 协调。
```

Code Agent 不得依据该残留句改回 Swift-only。下一版主规格应删除该句并替换为与 §1.3 一致的裁决。

## ERRATA 处理规则

- 此文件只登记明确、可定位的源文档矛盾。
- 不允许在这里扩展新需求。
- 新发现的冲突必须先登记，再通过 ADR 决定；不得静默解释。

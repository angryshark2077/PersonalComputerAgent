# Contract Registry

机器可读合同是跨 Rust、Swift、Cloud 和 Web 的共同基线。

## Rules

- JSON Schema Draft 2020-12.
- Breaking change requires protocol/schema version increment.
- Rust/Swift/TypeScript generated or mapped DTO must pass the same fixture.
- Unknown safety-critical fields/enums are rejected.
- Schemas do not carry secrets.
- The schemas in this pack are S0 baseline; implementation may only refine them through ADR and synchronized updates.

## Files

- `event-envelope.schema.json`
- `bridge-envelope.schema.json`
- `sync-batch-request.schema.json`
- `sync-batch-response.schema.json`
- `command-envelope.schema.json`
- `wechat-provider-state.schema.json`
- `collector-state.schema.json`
- `error-envelope.schema.json`
- `system-metric-sampled.schema.json`
- `collector-status-changed.schema.json`
- `system-health-changed.schema.json`

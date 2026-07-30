# V0 Implementation Plan

## Delivery model

七个两周 Sprint。顺序可以在同一 Sprint 内并行，但不得跳过前置门禁。

```text
S0 Engineering Baseline
  ↓
S1 Rust Core + Swift Bridge
  ↓
S2 Core Collectors
  ↓
S3 Cloud + Sync
  ↓
S4 Dashboard Core
  ↓
S5 Extended Sources + WeChat
  ↓
S6 Privacy + Update + Beta
```

## Sprint summary

| Sprint | Theme | Main delivery | Exit gate |
|---|---|---|---|
| S0 | Engineering baseline | Cargo/Xcode/pnpm Monorepo, CI, Contracts, AGENTS/CLAUDE, ADR, tokens | Rust/Swift/TS empty projects build; Bridge fixture round-trip |
| S1 | Rust Core + Swift Bridge | agentd, DbActor, Event Bus, SMAppService, Bridge, Keychain, Migration, Heartbeat | auto-start after login; Bridge crash recovery; SQLite durable |
| S2 | Core collection | Activity, System, Screenshot, privacy rules, Outbox | 2h offline no loss; permission revoke stops within 5s |
| S3 | Cloud and sync | Better Auth, Pairing, Hono, Postgres, R2, Batch Sync | duplicate upload idempotent; object upload complete |
| S4 | Dashboard core | Overview, Timeline, Screenshots, Activity, Devices/Commands | remote screenshot E2E; 10k virtual Timeline |
| S5 | Extended sources + WeChat | Browser, File, Location, Rust WeChatProvider, 4.1.12 Spike | no UI while logged out; auto ACTIVE or explainable unsupported; never restart WeChat |
| S6 | Privacy, update, Beta | Retention, Delete/Export, Sparkle, Migration Recovery, Sentry, audit | signed update; deleted data never resurrects; Beta acceptance |

## Workstream dependencies

- Contracts must exist before Rust/Swift/Cloud/Web implementations.
- Local Migration and Event Envelope must exist before Collector work.
- Device Pairing and Batch Sync must exist before remote Command E2E.
- WeChat Provider can Spike earlier, but production integration remains S5.
- Sparkle packaging work may start in S1/S3, but release gate belongs to S6.
- UI may use fixtures in S0-S3; it cannot define new backend semantics.

## Scope control

延后：

- AI summary, semantic search, personal Memory
- Windows/Linux Agent
- RustDesk/remote desktop/Computer Use
- WeChat send/reply
- full Safari URL collector
- E2EE multi-device key system
- employee monitoring/admin policy
- mobile system monitoring

新增上述能力必须进入独立版本路线，不得塞入 V0 Sprint。

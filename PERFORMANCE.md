# Performance Budgets

| Area | V0 Budget | Verification |
|---|---:|---|
| Agent idle CPU | average < 1%; near 0 without active collectors | Activity Monitor + signpost |
| Agent idle memory | < 120 MB | process sampling |
| Start to heartbeat | < 5 s; must not wait for WeChat/browser | startup trace |
| Activity event latency | < 2 s | event fixture |
| Remote screenshot visible | p95 < 15 s when online | E2E |
| Common SQLite writes | p95 < 20 ms | query timer |
| Sync batch | max 200 events or 1 MB | load test |
| Timeline 24h API | p95 < 300 ms | APM |
| Web Timeline | 10k items virtualized without obvious jank | Playwright performance |

## Mandatory implementation rules

- SQLite writes go through one DbActor/owned writer.
- Transactions remain short.
- No unbounded channels.
- Backpressure pauses low-priority System/Window events before critical events.
- Outbox > 10,000 triggers degradation policy.
- Disk available < 2 GB stops screenshots/attachment downloads while preserving metadata.
- Blocking database/provider work runs outside async executor threads.
- WeChat SQLCipher derived keys are cached by DB salt.
- Provider watch loop must debounce file/WAL changes.
- Dashboard lists use cursor pagination or virtualization.

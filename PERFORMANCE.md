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

## S1B control-plane budget

- A paired Agent attempts control immediately at startup and every 30 seconds
  while healthy; each HTTPS request has a 15-second timeout.
- Transient failure uses bounded jittered exponential retry, capped at five
  minutes. It must not block local S1A startup or create a retry queue.
- Pairing callback lifetime is five minutes and has exactly one terminal
  result. No periodic S1B sampling or Event upload is introduced by this
  slice.
- These are implementation bounds. A live start-to-heartbeat measurement is a
  deployment acceptance check after a real HTTPS origin is configured; it is
  not claimed by the current in-memory/temporary-PostgreSQL gates.
- The local Railway preparation gate validates the offline public-health
  verifier and migration/build contracts only. It does not measure a live
  Singapore Railway API, Dashboard proxy, PostgreSQL migration, or
  Setup-to-Agent handoff; those remain operator-run acceptance measurements.

## Mandatory implementation rules

- SQLite writes go through one DbActor/owned writer.
- Transactions remain short.
- No unbounded channels.
- Backpressure pauses low-priority System/Window events before critical events.
- Active Outbox depth > 10,000 suppresses System sampling; depth = 10,000 remains active.
- Suppressed System sampling resumes only below 8,000; depth = 8,000 remains suppressed.
- Disk available < 2 GB stops screenshots/attachment downloads while preserving metadata.
- Blocking database/provider work runs outside async executor threads.
- WeChat SQLCipher derived keys are cached by DB salt.
- Provider watch loop must debounce file/WAL changes.
- Dashboard lists use cursor pagination or virtualization.

## System Collector acceptance

- CPU/Memory starts immediately and then samples every 30 seconds; Disk starts immediately and then samples every 5 minutes.
- Sampling suppression and retry do not fabricate catch-up Events.
- The local verifier discards a 10-second warm-up, then samples the exact Agent child with `/bin/ps` every 5 seconds for 60 seconds.
- Acceptance requires average CPU strictly below 1.0% and peak RSS strictly below 122,880 KiB.
- Run on the target Apple Silicon Mac: `cargo build -p pca-agentd --features process-test-hooks && python3 scripts/verify-s2-system-performance.py --agent target/debug/pca-agentd`.
- The timing-sensitive 60-second probe is completion evidence and is intentionally not a shared CI gate.

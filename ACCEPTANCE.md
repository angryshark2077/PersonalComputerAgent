# V0 Acceptance Matrix

## Product scenarios

| Scenario | Pass condition |
|---|---|
| First install | pairing, background registration and Activity permission complete; first Web event within 5 minutes |
| Continuous collection | 24h without fatal crash; resumes after sleep/wake |
| Offline recovery | 2h offline then sync in time order without duplicates |
| Remote screenshot | online device returns visible screenshot within 15s; permission failure has explicit code |
| Privacy pause | stops new sensitive events within 5s; no fabricated catch-up for paused time |
| Delete | Dashboard/search/object storage cleaned; offline device cannot resurrect |
| WeChat | supported version incremental; unsupported does not affect agent core |
| Update | check/download/backup/install/migrate/restart; failure recovers |

## Technical acceptance

| Domain | Pass condition |
|---|---|
| Security | Keychain, permission gates, signed update, no secrets in logs, Workspace scope tests |
| Stability | Agent/Bridge/Adapter crash recovery; Outbox durable; timed-out child process terminated |
| Data | Cloud/Local migrations from zero and previous versions; Event/Projection consistency |
| Performance | all budgets in `PERFORMANCE.md` |
| Observability | every error has code and request/command/batch correlation |
| Replaceability | WeChat, Storage, Map and Browser behind contracts |
| Documentation | AGENTS, CLAUDE, Data Dictionary, OpenAPI, ADR match code |

## Release blocker rule

Any failed item in:

- permission revocation
- Keychain secret storage
- tombstone resurrection
- update signature
- migration recovery
- cross-workspace isolation
- WeChat normal-path no-interference

blocks Beta release.

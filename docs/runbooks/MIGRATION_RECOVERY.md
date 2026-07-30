# Local SQLite Migration Recovery

1. Stop new Collector/Provider writes.
2. Flush DbActor queue.
3. WAL checkpoint.
4. Create SQLite Backup API snapshot.
5. Acquire exclusive migration lock.
6. Apply immutable migrations in order.
7. Run:
   - `PRAGMA integrity_check`
   - `PRAGMA foreign_key_check`
   - key table/index smoke tests
8. Write migration completion marker.
9. Resume agent.
10. On failure:
    - stop startup
    - restore backup
    - preserve sanitized diagnostics
    - enter repair state
    - never continue on partially migrated DB

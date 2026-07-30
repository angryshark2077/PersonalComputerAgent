# S4 · Dashboard Core

## Must read

Spec §5, §7-10, §22, §23, §27.

## Objective

Deliver usable Web Dashboard for core data and remote screenshot command.

## Deliverables

- global Workspace/Device/Date scope.
- Overview.
- virtualized Timeline.
- Screenshot Explorer.
- Activity.
- Devices and permission/collector health.
- remote screenshot command.
- loading/empty/error/degraded states.
- Design Tokens.
- query cursor/pagination.
- delete/export entry points as disabled skeleton where backend is not ready.

## Rules

- Web UI never imports Drizzle.
- UI cannot infer command success before terminal result.
- state color must have text/icon.
- raw payload only in developer panel.
- 10k Timeline items virtualized.
- sensitive previews obey authorization and deletion.

## Tests

- filters and URL scope.
- command queued→running→succeeded/failed.
- 10k Timeline performance.
- screenshot preview/delete state.
- device stale/sleeping/offline.
- API error envelope mapping.
- keyboard/accessibility paths.

## Exit gate

- remote screenshot E2E visible within p95 15s online.
- Timeline 10k items smooth.
- all core states covered.

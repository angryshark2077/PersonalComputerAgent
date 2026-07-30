# S0 · Engineering Baseline

## Must read

- Spec §1.2-1.4
- §12.1-12.3
- §13.1-13.5
- §18
- §22
- §24
- §25
- Appendix C, D, F, G, H, I
- `docs/SOURCE_ERRATA.md`

## Objective

Create a buildable Monorepo and freeze machine-readable contracts before business implementation.

## Deliverables

- Cargo workspace and Rust crate boundaries.
- Swift package/project boundaries for BridgeProtocol and Setup/Repair.
- pnpm workspace for Cloud/Web/contracts.
- root AGENTS/CLAUDE/ARCHITECTURE/SECURITY/PERFORMANCE.
- JSON Schema baseline:
  - Event
  - Bridge request/response
  - Sync batch
  - Command
  - Provider state
  - Error envelope
- generated-model strategy documented.
- protocol version strategy.
- local/cloud migration directories and migration metadata format.
- CI stages:
  - format
  - lint
  - build
  - unit
  - contract
  - migration
  - boundary
- dependency boundary rules.
- test fixture directory and one Rust↔Swift Bridge round-trip fixture.
- ADR index.

## Non-goals

- No real Collector.
- No real WeChat scan.
- No Cloud deployment.
- No Dashboard feature page.
- No Sparkle production feed.

## Implementation sequence

1. Inspect repository.
2. Create package graph and public exports.
3. Add JSON Schema and enum registry.
4. Generate or hand-map minimal Rust/Swift fixture DTO.
5. Add fixture round-trip test.
6. Add CI and boundary tests.
7. Add empty local/cloud migration baseline.
8. Run all gates.

## Required tests

- JSON schemas parse.
- Every enum in Appendix C is registered once.
- Rust Event fixture round-trip.
- Swift Bridge fixture round-trip.
- Unknown `protocol_version` rejected.
- Missing `request_id` rejected.
- Workspace builds in clean checkout.
- Domain crate cannot import platform/infrastructure crates.

## Exit gate

- Rust/Swift/TS empty projects build.
- Bridge fixture round-trip passes on both sides.
- CI runs all baseline stages.
- No architecture contradiction remains undocumented.

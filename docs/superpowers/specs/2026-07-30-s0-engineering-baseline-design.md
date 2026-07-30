# S0 Engineering Baseline Design

## Status

Approved for written-spec review on 2026-07-30. Implementation must not begin until the user approves this file.

## Goal

Preserve `/Users/jacob/Projects/PCA` as the authoritative development pack and create `/Users/jacob/Projects/PersonalComputerAgent` as an independent Git repository whose S0 engineering baseline is buildable, contract-tested, and unable to report a false full-pass result.

## Scope

S0 establishes the repository, contracts, empty language projects, migrations, CI, and verification gates required before S1. It does not implement product collectors or runtime behavior.

Included:

- Repair the development pack bootstrap and generated-repository documentation.
- Make JSON Schema the cross-language wire-contract authority.
- Provide explicit Rust, Swift, and TypeScript contract mappings.
- Add contract fixtures and Rust/Swift round-trip tests.
- Register Appendix C enums and Appendix D error codes exactly once.
- Provide empty local SQLite and cloud PostgreSQL migration baselines.
- Provide Rust, Swift, and TypeScript build/typecheck/test commands.
- Add dependency-boundary checks and CI stages.
- Distinguish structural validation from full engineering verification.
- Generate and initialize `/Users/jacob/Projects/PersonalComputerAgent` as a Git repository.

Excluded:

- Real Screenshot, Activity, System, Browser, File, Location, or Communication collectors.
- WeChat process scanning, key extraction, SQLCipher access, or fixtures containing user data.
- Cloud deployment, authentication implementation, object storage, or production API behavior.
- Dashboard feature pages.
- S1 resident-agent behavior, SMAppService runtime registration, heartbeat workers, durable Event Store, or Outbox execution.

## Directory Ownership

### Development pack

`/Users/jacob/Projects/PCA` remains the source for:

- Product and technical specifications.
- Sprint task definitions and engineering rules.
- Canonical JSON Schema files.
- Repository template files.
- Bootstrap and pack-verification scripts.

It is not the application repository and will not contain application runtime data.

### Product repository

`/Users/jacob/Projects/PersonalComputerAgent` is generated from the repaired development pack and becomes the only repository used for S1-S6 implementation. It will contain a fresh `.git` directory and an initial S0 commit history; no history is copied from the pack because the pack is not currently a Git repository.

## Canonical Contracts

The canonical contract files are JSON Schema draft 2020-12 documents. S0 uses small, explicit hand-mapped Rust, Swift, and TypeScript types; each mapping must conform to the schemas and must not redefine wire names. Automated fixture tests enforce the mapping. Code generation is deferred until contract volume justifies an additional generator dependency.

S0 freezes these wire conventions:

- JSON property names use `snake_case`.
- Bridge deadlines use the wire name `deadline_ms`.
- `payload` is a JSON object, not a Base64-encoded byte string.
- Identifiers described as UUIDs are validated as UUID strings.
- Protocol version rejection is tested explicitly.
- Missing required fields are rejected explicitly.
- Unknown fields are rejected where the schema sets `additionalProperties: false`.

Swift types use explicit `CodingKeys`. Rust types use `serde` derives and field types compatible with the schema. TypeScript types use explicit interfaces checked by the shared fixture tests. No language implementation becomes a second contract authority.

## Enum and Error Registry

Appendix C enums and Appendix D error codes are represented in one machine-readable registry inside the contracts package. Validation checks compare that registry with language mappings and schema enums. A value may appear in multiple generated outputs but has one registry definition.

S0 does not invent additional product states or error codes. Any required addition must first update the authoritative specification or errata according to the existing fact-source rules.

## Fixture Strategy

Fixtures contain synthetic, non-sensitive data only. The minimum fixture set covers:

- A valid Bridge request.
- A valid Bridge response.
- An incompatible `protocol_version` case.
- A missing `request_id` case.
- A valid Event envelope.
- An Event containing optional attachment and idempotency fields.

Rust and Swift tests decode the same Bridge fixture, assert semantic values, re-encode it, and validate the result against the schema. Rust and TypeScript tests perform the equivalent Event checks where applicable.

## Repository Baseline

The product repository contains focused empty packages matching the architectural dependency direction:

- Rust workspace: agent core, domain, provider contracts, local DB baseline, and contract tests.
- Swift package: BridgeProtocol, PlatformBridge placeholder, Setup/Repair placeholder, and tests.
- pnpm workspace: contracts, domain TypeScript, cloud API placeholder, and web-dashboard placeholder.
- Migration roots: immutable local SQLite baseline and cloud PostgreSQL baseline.
- CI configuration: format, lint, build, unit, contract, migration, and boundary stages.

Empty projects must compile but must not simulate finished business behavior.

## Bootstrap Behavior

The pack bootstrap accepts one absolute target directory. It refuses a non-empty target, copies every required source-of-truth and repository file, and leaves generated documentation with paths valid from the generated repository root.

The generated repository includes `DEV_PACKAGE_MANIFEST.md`. It does not instruct the user to run a bootstrap script that is absent from the generated repository, and every onboarding path is relative to the generated repository root.

Bootstrap tests run against temporary directories and verify required files, paths, contract copies, and rejection of non-empty targets.

## Verification Modes

Two modes are explicit:

### Structural verification

Checks file presence, JSON parsing, schema metadata and references, bootstrap consistency, and shell syntax. It may run without Rust, Swift, Node, or pnpm. Its final output must say `STRUCTURAL VERIFICATION PASSED` and must never imply the full engineering gate passed.

### Full verification

Requires every declared toolchain and runs:

- Rust format, clippy with warnings denied, build, and tests.
- Swift build and tests with Swift 6 strict-concurrency compatibility.
- TypeScript install consistency, typecheck, and tests.
- JSON Schema and fixture validation.
- Local and cloud migration baseline/replay checks.
- Dependency-boundary checks.
- Bootstrap smoke test from a clean temporary directory.

Any missing toolchain or failed stage makes full verification exit non-zero. Its final output says `FULL VERIFICATION PASSED` only after every stage exits successfully.

## Error Handling

- Bootstrap failures use stable non-zero exit codes and identify the exact invalid target or missing source file.
- Contract validation reports the filename and failing schema path.
- Boundary checks report the forbidden dependency edge.
- Migration checks report the migration identifier and checksum mismatch.
- Verification scripts preserve the failing child-process exit code.
- No verification path catches an error and continues to a full-pass message.

## Testing Strategy

Behavioral changes follow red-green TDD. Configuration-only files are verified by the first behavioral test that consumes them.

Required automated evidence:

- Bootstrap test fails against the current missing-file/path behavior, then passes after repair.
- Structural/full mode test demonstrates that a missing Cargo toolchain cannot produce a full-pass message.
- JSON Schema validation catches invalid instances, not only invalid JSON syntax.
- Swift Bridge fixture test fails against camelCase/Data encoding before the mapping fix.
- Rust Event fixture test fails against the current `payload_json: String` representation before the mapping fix.
- Enum registry completeness test covers all Appendix C groups.
- Migration replay starts from empty local and cloud baselines.
- Boundary test prevents domain packages from importing platform or infrastructure packages.
- Clean generated repository passes every available full gate.

## Acceptance Criteria

S0 is complete only when:

1. The development pack bootstrap generates a self-consistent repository in a clean temporary directory.
2. `/Users/jacob/Projects/PersonalComputerAgent` exists as an independent Git repository generated from the repaired pack.
3. Rust, Swift, and TypeScript empty projects build from a clean checkout.
4. Shared Bridge fixtures pass Rust and Swift semantic round trips.
5. Event fixtures pass schema and language mapping tests.
6. Every Appendix C enum is registered once and every Appendix D error code is present in the registry.
7. Local and cloud migration baselines replay from empty state.
8. CI executes format, lint, build, unit, contract, migration, and boundary gates.
9. Structural verification and full verification have distinct output and exit semantics.
10. No real Collector, WeChat access, cloud deployment, or Dashboard feature implementation is included.

## Rollback

Changes to the pack are surgical and reviewable by file. The generated product repository is new and contains no user data; before S1, rollback consists of reverting pack commits when available and regenerating the product repository from the last accepted pack state. No existing important directory is overwritten.

## Known Environment Constraint

At design time, Swift 6.3.3 and pnpm are available, but Cargo is not available on the machine path. Full S0 verification therefore requires installing or exposing the Rust toolchain. Structural verification may proceed earlier but cannot close S0.

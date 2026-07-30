# S0 Engineering Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Repair the PCA development pack and create `/Users/jacob/Projects/PersonalComputerAgent` as an independent, buildable, contract-tested S0 Git repository.

**Architecture:** `/Users/jacob/Projects/PCA` remains the specification and scaffold authority. Bootstrap first creates the product repository; S0 implementation proceeds there with small commits, then the verified baseline is synchronized back into `PCA/repo-template` and validated by generating a clean temporary repository.

**Tech Stack:** Bash, Python 3 standard library, Rust stable 1.82+ with Serde, Swift 6, pnpm 9.15.0, TypeScript 5, Ajv 8, GitHub Actions.

## Global Constraints

- V0 is Web-dashboard-first, Rust Core, Swift macOS Bridge, event-driven, and privacy-by-design.
- Rust `agentd` is the local runtime and business-state authority; Swift exposes Apple capabilities only.
- Collectors emit Events and never call Cloud APIs directly.
- JSON Schema draft 2020-12 is the cross-language wire-contract authority.
- Wire property names use `snake_case`; Bridge payloads are JSON objects.
- Secrets never enter Event payloads, ordinary SQLite tables, logs, fixtures, or diagnostic bundles.
- Rust workspace minimum version is 1.82; `unsafe_code` remains denied.
- Swift uses Swift 6 language mode and Sendable-safe contract types.
- TypeScript enables `strict`, `noUncheckedIndexedAccess`, and `exactOptionalPropertyTypes`.
- S0 adds no real Collector, WeChat access, cloud deployment, Dashboard feature page, or S1 resident runtime.
- `/Users/jacob/Projects/PCA` is not a Git repository; commits begin only after the product repository is created.

---

### Task 1: Repair Bootstrap and Create the Product Repository

**Files:**
- Create: `/Users/jacob/Projects/PCA/scripts/tests/test_bootstrap.py`
- Modify: `/Users/jacob/Projects/PCA/scripts/bootstrap-repo.sh`
- Modify: `/Users/jacob/Projects/PCA/00_START_HERE.md`
- Modify: `/Users/jacob/Projects/PCA/repo-template/README.md`
- Create through bootstrap: `/Users/jacob/Projects/PersonalComputerAgent/**`

**Interfaces:**
- Consumes: one absolute target path passed to `scripts/bootstrap-repo.sh`.
- Produces: a self-contained generated repository with `DEV_PACKAGE_MANIFEST.md`, valid root-relative onboarding links, specifications, contracts, tasks, prompts, and template source files.

- [x] **Step 1: Write the failing bootstrap behavior tests**

```python
import subprocess
import tempfile
import unittest
from pathlib import Path

PACK = Path(__file__).resolve().parents[2]
BOOTSTRAP = PACK / "scripts" / "bootstrap-repo.sh"


class BootstrapTests(unittest.TestCase):
    def test_generated_repository_is_self_contained(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "PersonalComputerAgent"
            result = subprocess.run(
                [str(BOOTSTRAP), str(target)], text=True, capture_output=True
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((target / "DEV_PACKAGE_MANIFEST.md").is_file())
            self.assertTrue((target / "docs/PRODUCT_TECH_SPEC_V1.1.md").is_file())
            readme = (target / "README.md").read_text(encoding="utf-8")
            self.assertIn("00_START_HERE.md", readme)
            self.assertNotIn("../00_START_HERE.md", readme)
            start = (target / "00_START_HERE.md").read_text(encoding="utf-8")
            self.assertNotIn("./scripts/bootstrap-repo.sh", start)

    def test_non_empty_target_is_rejected_without_overwrite(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "existing"
            target.mkdir()
            sentinel = target / "keep.txt"
            sentinel.write_text("keep", encoding="utf-8")
            result = subprocess.run(
                [str(BOOTSTRAP), str(target)], text=True, capture_output=True
            )
            self.assertEqual(result.returncode, 3)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()
```

- [x] **Step 2: Run the tests and verify the first test fails for the current missing manifest and bad paths**

Run:

```bash
python3 -m unittest scripts.tests.test_bootstrap -v
```

Expected: `test_generated_repository_is_self_contained` fails because `DEV_PACKAGE_MANIFEST.md` is absent and generated onboarding still contains invalid paths; the non-empty-target test passes.

- [x] **Step 3: Implement the minimum bootstrap and onboarding repair**

Add `DEV_PACKAGE_MANIFEST.md` to the explicit bootstrap copy list. Replace the generated-repository portion of `00_START_HERE.md` with root-valid commands and remove the instruction to bootstrap again. Change `repo-template/README.md` to:

```markdown
# Personal Computer Agent

This repository was initialized from the Code Agent Development Pack.

Start with:

- `00_START_HERE.md`
- `tasks/S0_ENGINEERING_BASELINE.md`

The scaffold is intentionally minimal and does not represent completed product functionality.
```

- [x] **Step 4: Run the bootstrap tests and shell syntax checks**

Run:

```bash
python3 -m unittest scripts.tests.test_bootstrap -v
bash -n scripts/bootstrap-repo.sh scripts/verify-pack.sh
```

Expected: two tests pass and Bash exits 0.

- [x] **Step 5: Generate the product repository and initialize Git**

Run only after confirming `/Users/jacob/Projects/PersonalComputerAgent` does not exist or is empty:

```bash
./scripts/bootstrap-repo.sh /Users/jacob/Projects/PersonalComputerAgent
git -C /Users/jacob/Projects/PersonalComputerAgent init
git -C /Users/jacob/Projects/PersonalComputerAgent add .
git -C /Users/jacob/Projects/PersonalComputerAgent commit -m "chore: initialize PCA repository"
```

Expected: bootstrap exits 0; Git reports a new repository and the initial commit succeeds.

---

### Task 2: Establish the Contract Registry, Fixtures, and TypeScript Baseline

**Files:**
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/src/types.ts`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/src/validate.ts`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/tests/contracts.test.ts`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/registry.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/fixtures/bridge-request.valid.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/fixtures/bridge-response.valid.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/fixtures/bridge-request.incompatible.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/fixtures/bridge-request.missing-request-id.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/fixtures/event.valid.json`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/package.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/contracts/tsconfig.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/domain-ts/package.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/domain-ts/src/index.ts`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/apps/web-dashboard/package.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/apps/web-dashboard/src/index.ts`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/tsconfig.base.json`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/package.json`

**Interfaces:**
- Consumes: `packages/contracts/*.schema.json` and Appendix C/D values.
- Produces: `validateContract(schemaName, value): { valid: boolean; errors: string[] }`, shared fixture files, `BridgeEnvelope`, `EventEnvelope`, and the single enum/error registry.

- [x] **Step 1: Add synthetic fixtures and write failing Ajv behavior tests**

Use literal UUIDs and UTC timestamps. The valid Bridge request must contain:

```json
{
  "protocol_version": 1,
  "request_id": "018f3f4a-2d9b-7d21-a310-2c49d9b43c11",
  "message_kind": "request",
  "capability": "system.capabilities",
  "deadline_ms": 1000,
  "payload": {"include_permissions": true}
}
```

The contract test must assert real validation behavior:

```typescript
import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { validateContract } from "../src/validate.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (name: string) =>
  JSON.parse(readFileSync(join(here, "../fixtures", name), "utf8"));

test("valid Bridge request satisfies the canonical schema", () => {
  assert.deepEqual(validateContract("bridge-envelope", fixture("bridge-request.valid.json")), {
    valid: true,
    errors: [],
  });
});

test("Bridge request without request_id is rejected", () => {
  const result = validateContract(
    "bridge-envelope",
    fixture("bridge-request.missing-request-id.json"),
  );
  assert.equal(result.valid, false);
  assert.match(result.errors.join("\n"), /request_id/);
});

test("fixture payload is an object rather than encoded bytes", () => {
  const value = fixture("bridge-request.valid.json");
  assert.equal(Array.isArray(value.payload), false);
  assert.equal(typeof value.payload, "object");
});
```

- [x] **Step 2: Run the tests and verify RED**

Run:

```bash
cd /Users/jacob/Projects/PersonalComputerAgent
pnpm install
pnpm --filter @pca/contracts test
```

Expected: fail because `src/validate.ts`, scripts, and Ajv integration do not yet exist.

- [x] **Step 3: Implement the minimum TypeScript contract validator and explicit types**

Implement `validate.ts` with one Ajv 2020 instance, schema registration by `$id`, and formatted `instancePath + message` errors. Use:

```typescript
export interface ValidationResult {
  valid: boolean;
  errors: string[];
}

export function validateContract(
  schemaName: ContractSchemaName,
  value: unknown,
): ValidationResult;
```

Define explicit wire types in `types.ts` using `snake_case`, including:

```typescript
export interface BridgeEnvelope {
  protocol_version: number;
  request_id: string;
  message_kind: "request" | "response" | "event";
  capability: string;
  deadline_ms: number;
  payload: Record<string, unknown>;
  error?: ErrorEnvelope | null;
}
```

Create `registry.json` with all 16 Appendix C enum groups and all Appendix D error codes. Tests must assert the literal group count of 16 and reject duplicate values within a group.

- [x] **Step 4: Add the minimal pnpm workspace projects**

Set the root scripts to:

```json
{
  "typecheck": "pnpm -r typecheck",
  "test": "pnpm -r test",
  "verify:contracts": "pnpm --filter @pca/contracts test"
}
```

Use the shared `tsconfig.base.json`:

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "exactOptionalPropertyTypes": true,
    "noEmit": true
  }
}
```

Web Dashboard and domain-ts remain importable empty packages; they expose only a package identifier and no feature UI.

- [x] **Step 5: Run TypeScript verification**

Run:

```bash
pnpm typecheck
pnpm test
```

Expected: all workspace typechecks and contract tests pass with zero failures.

- [x] **Step 6: Commit**

```bash
git add package.json pnpm-lock.yaml tsconfig.base.json apps packages
git commit -m "feat: freeze TypeScript contract baseline"
```

---

### Task 3: Map Canonical Event and Bridge Contracts in Rust

**Files:**
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/Cargo.toml`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/crates/domain/Cargo.toml`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/crates/domain/src/lib.rs`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/crates/test-contracts/Cargo.toml`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/crates/test-contracts/src/lib.rs`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/crates/test-contracts/tests/fixtures.rs`

**Interfaces:**
- Consumes: shared JSON fixtures under `packages/contracts/fixtures`.
- Produces: Serde-compatible `EventEnvelope`, `BridgeEnvelope`, `ErrorEnvelope`, and fixture round-trip tests.

- [x] **Step 1: Write failing Rust fixture tests**

```rust
use pca_domain::{BridgeEnvelope, EventEnvelope};

#[test]
fn bridge_fixture_decodes_snake_case_and_object_payload() {
    let raw = include_str!(
        "../../../packages/contracts/fixtures/bridge-request.valid.json"
    );
    let envelope: BridgeEnvelope = serde_json::from_str(raw).expect("valid bridge fixture");
    assert_eq!(envelope.protocol_version, 1);
    assert_eq!(envelope.deadline_ms, 1_000);
    assert_eq!(envelope.payload["include_permissions"], true);
}

#[test]
fn event_fixture_round_trips_without_field_loss() {
    let raw = include_str!("../../../packages/contracts/fixtures/event.valid.json");
    let event: EventEnvelope = serde_json::from_str(raw).expect("valid event fixture");
    let encoded = serde_json::to_value(event).expect("encode event");
    assert_eq!(encoded["payload"]["state"], "running");
    assert_eq!(encoded["attachment_refs"][0], "attachment-001");
}
```

- [x] **Step 2: Run the tests and verify RED**

Run:

```bash
cargo test -p pca-test-contracts --test fixtures
```

Expected: compilation fails because the current domain model lacks Serde mappings, `BridgeEnvelope`, object payloads, and optional contract fields.

- [x] **Step 3: Implement the minimum Rust mappings**

Add workspace dependencies for `serde`, `serde_json`, and `uuid`. Replace `payload_json: String` with `payload: serde_json::Map<String, serde_json::Value>`, add optional `attachment_refs` and `idempotency_key`, and derive `Serialize`/`Deserialize` using `#[serde(rename_all = "snake_case")]` for string enums.

Use concrete Bridge fields:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeEnvelope {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub message_kind: BridgeMessageKind,
    pub capability: String,
    pub deadline_ms: u64,
    pub payload: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorEnvelope>,
}
```

Do not add runtime sockets, Tokio, SQLite, or Collector implementations in S0.

- [x] **Step 4: Run Rust gates**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit 0 with no warnings.

- [x] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates agent
git commit -m "feat: map canonical contracts in Rust"
```

---

### Task 4: Map and Round-Trip the Bridge Contract in Swift

**Files:**
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/platform/macos/Package.swift`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/platform/macos/Sources/BridgeProtocol/BridgeEnvelope.swift`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/platform/macos/Sources/BridgeProtocol/JSONValue.swift`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/platform/macos/Tests/BridgeProtocolTests/BridgeEnvelopeTests.swift`

**Interfaces:**
- Consumes: the shared `packages/contracts/fixtures/bridge-request.valid.json` directly through a path derived from `#filePath`; no duplicate Swift fixture is created.
- Produces: a Swift `BridgeEnvelope` that decodes and re-encodes the canonical snake_case wire contract without field loss.

- [x] **Step 1: Write the failing Swift fixture test**

```swift
import Foundation
import Testing
@testable import BridgeProtocol

@Test func bridgeFixtureUsesCanonicalWireKeys() throws {
    let sourceFile = URL(fileURLWithPath: #filePath)
    let repositoryRoot = (0..<5).reduce(sourceFile) { url, _ in
        url.deletingLastPathComponent()
    }
    let url = repositoryRoot
        .appendingPathComponent("packages/contracts/fixtures")
        .appendingPathComponent("bridge-request.valid.json")
    let data = try Data(contentsOf: url)
    let value = try JSONDecoder().decode(BridgeEnvelope.self, from: data)
    #expect(value.protocolVersion == 1)
    #expect(value.deadlineMilliseconds == 1_000)
    #expect(value.payload["include_permissions"] == .bool(true))

    let encoded = try JSONEncoder().encode(value)
    let object = try #require(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
    #expect(object["protocol_version"] as? Int == 1)
    #expect(object["deadline_ms"] as? Int == 1_000)
    #expect(object["payload"] is [String: Any])
    #expect(object["protocolVersion"] == nil)
}
```

- [x] **Step 2: Run the test and verify RED**

Run:

```bash
swift test --package-path platform/macos
```

Expected: fail because the current Swift model expects camelCase wire keys and encodes `payload` as Base64 `Data`.

- [x] **Step 3: Implement the minimum Sendable JSON contract mapping**

Implement a recursive `JSONValue: Codable, Sendable, Equatable` enum for null, bool, number, string, array, and object. Change payload to `[String: JSONValue]`. Add explicit `CodingKeys`:

```swift
private enum CodingKeys: String, CodingKey {
    case protocolVersion = "protocol_version"
    case requestID = "request_id"
    case messageKind = "message_kind"
    case capability
    case deadlineMilliseconds = "deadline_ms"
    case payload
    case error
}
```

Add the `BridgeProtocolTests` test target in `Package.swift`. Keep Setup/Repair as a placeholder and read the single shared fixture from its canonical repository path.

- [x] **Step 4: Run Swift gates**

```bash
swift build --package-path platform/macos
swift test --package-path platform/macos
```

Expected: both commands exit 0 and the shared fixture test passes.

- [x] **Step 5: Commit**

```bash
git add platform/macos
git commit -m "feat: map canonical Bridge contract in Swift"
```

---

### Task 5: Add Migration Baselines and Dependency-Boundary Checks

**Files:**
- Create: `/Users/jacob/Projects/PersonalComputerAgent/crates/db-local/Cargo.toml`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/crates/db-local/src/lib.rs`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/crates/db-local/migrations/0000_baseline.sql`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/db-cloud/package.json`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/packages/db-cloud/migrations/0000_baseline.sql`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/scripts/verify_migrations.py`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/scripts/verify_boundaries.py`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/scripts/tests/test_engineering_gates.py`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/Cargo.toml`

**Interfaces:**
- Consumes: immutable `0000_baseline.sql` files and workspace manifests/source imports.
- Produces: deterministic migration checksum verification and non-zero boundary failures identifying the forbidden edge.

- [x] **Step 1: Write failing gate tests against controlled temporary fixtures**

```python
def test_duplicate_migration_id_is_rejected(self) -> None:
    root = self.make_repo()
    self.write(root / "crates/db-local/migrations/0000_a.sql", "SELECT 1;")
    self.write(root / "crates/db-local/migrations/0000_b.sql", "SELECT 2;")
    result = self.run_gate("verify_migrations.py", root)
    self.assertNotEqual(result.returncode, 0)
    self.assertIn("duplicate migration id: 0000", result.stderr)

def test_domain_to_platform_import_is_rejected(self) -> None:
    root = self.make_repo()
    self.write(root / "crates/domain/src/lib.rs", "use pca_platform::Bridge;")
    result = self.run_gate("verify_boundaries.py", root)
    self.assertNotEqual(result.returncode, 0)
    self.assertIn("crates/domain -> platform", result.stderr)
```

The test helper creates only the directories and files needed for each real script invocation; expected strings are literals, not derived from the gate implementation.

- [x] **Step 2: Run and verify RED**

```bash
python3 -m unittest scripts.tests.test_engineering_gates -v
```

Expected: tests fail because both gate scripts are absent.

- [x] **Step 3: Implement minimal baseline migrations and gates**

Local baseline creates only `schema_migrations` with immutable identifiers/checksums; cloud baseline creates only a `_pca_migrations` metadata table. `verify_migrations.py` must:

- Accept an optional repository root argument.
- Require exactly one `0000_baseline.sql` in each migration root.
- Reject duplicate numeric prefixes.
- Execute the local `CREATE TABLE IF NOT EXISTS schema_migrations` baseline twice against a temporary SQLite database and require both executions to succeed without changing the table definition.
- Record and compare SHA-256 checksums.

`verify_boundaries.py` must inspect Cargo path dependencies and TypeScript imports, not merely grep documentation. It rejects domain-to-platform/infrastructure, collector-to-cloud-client, and web-to-db-cloud edges.

- [x] **Step 4: Run the gate tests and real repository checks**

```bash
python3 -m unittest scripts.tests.test_engineering_gates -v
python3 scripts/verify_migrations.py .
python3 scripts/verify_boundaries.py .
cargo test -p pca-db-local
```

Expected: all commands exit 0.

- [x] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/db-local packages/db-cloud scripts
git commit -m "feat: add S0 migration and boundary gates"
```

---

### Task 6: Replace False-Green Verification and Add CI

**Files:**
- Create: `/Users/jacob/Projects/PersonalComputerAgent/scripts/verify-structural.sh`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/scripts/verify-full.sh`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/scripts/tests/test_verification_modes.py`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/justfile`
- Create: `/Users/jacob/Projects/PersonalComputerAgent/.github/workflows/ci.yml`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/README.md`

**Interfaces:**
- Consumes: repository gates from Tasks 2-5 and available toolchains.
- Produces: `STRUCTURAL VERIFICATION PASSED` only for structural mode, `FULL VERIFICATION PASSED` only after every required engineering gate succeeds.

- [x] **Step 1: Write failing verification-mode tests**

```python
def test_structural_mode_never_claims_full_pass(self) -> None:
    result = self.run_script("verify-structural.sh")
    self.assertEqual(result.returncode, 0, result.stderr)
    self.assertIn("STRUCTURAL VERIFICATION PASSED", result.stdout)
    self.assertNotIn("FULL VERIFICATION PASSED", result.stdout)

def test_full_mode_fails_when_cargo_is_hidden(self) -> None:
    result = self.run_script("verify-full.sh", env={"PATH": self.path_without("cargo")})
    self.assertNotEqual(result.returncode, 0)
    self.assertIn("missing required tool: cargo", result.stderr)
    self.assertNotIn("FULL VERIFICATION PASSED", result.stdout)
```

- [x] **Step 2: Run and verify RED**

```bash
python3 -m unittest scripts.tests.test_verification_modes -v
```

Expected: fail because the scripts do not exist.

- [x] **Step 3: Implement structural and full verification scripts**

Both scripts use `set -euo pipefail`. Structural mode runs file, JSON/ref, shell, migration-metadata, boundary, and bootstrap-consistency checks that do not require compilation. Full mode first checks exact required tools, then runs:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
swift build --package-path platform/macos
swift test --package-path platform/macos
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test
python3 scripts/verify_migrations.py .
python3 scripts/verify_boundaries.py .
```

Print the full-pass marker only after the last command exits 0.

- [x] **Step 4: Add CI with explicit stages**

The GitHub Actions workflow uses macOS for Swift and Rust/Node compatibility. Jobs are named `format-lint`, `build-unit`, `contract`, `migration`, and `boundary`; each invokes the same repository commands rather than duplicating validation logic.

- [x] **Step 5: Run verification-mode tests and available gates**

```bash
python3 -m unittest scripts.tests.test_verification_modes -v
./scripts/verify-structural.sh
./scripts/verify-full.sh
```

Expected: mode tests and structural verification pass. Full verification must exit non-zero with `missing required tool: cargo` until Cargo is installed; it must not print the full-pass marker.

- [x] **Step 6: Install Rust through Homebrew rustup when Cargo is absent, then rerun full verification**

Do not edit `.env` or application secrets. If `command -v cargo` fails, run:

```bash
brew install rustup
rustup toolchain install stable --profile minimal --component rustfmt,clippy
rustup default stable
```

Then run:

```bash
rustc --version
cargo --version
./scripts/verify-full.sh
```

Expected: versions are printed and full verification exits 0 with exactly one `FULL VERIFICATION PASSED` marker.

- [x] **Step 7: Commit**

```bash
git add .github scripts justfile README.md
git commit -m "ci: enforce complete S0 verification"
```

---

### Task 7: Synchronize the Verified Baseline Back to the Pack and Prove Clean Generation

**Files:**
- Modify selectively: `/Users/jacob/Projects/PCA/repo-template/**`
- Modify: `/Users/jacob/Projects/PCA/contracts/*.schema.json` only if canonical schemas changed
- Modify: `/Users/jacob/Projects/PCA/scripts/verify-pack.sh`
- Modify: `/Users/jacob/Projects/PCA/DEV_PACKAGE_MANIFEST.md`
- Create: `/Users/jacob/Projects/PCA/scripts/tests/test_pack_verification.py`
- Modify: `/Users/jacob/Projects/PersonalComputerAgent/docs/superpowers/plans/2026-07-30-s0-engineering-baseline.md`

**Interfaces:**
- Consumes: the verified product-repository S0 skeleton.
- Produces: a pack template that regenerates an equivalent clean S0 repository and a final evidence record.

- [x] **Step 1: Write the failing pack-regeneration test**

The test bootstraps into a temporary directory, compares the canonical contract/fixture/tooling subset with the product repository, and runs structural verification in the generated target:

```python
def test_pack_regenerates_verified_s0_baseline(self) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        target = Path(tmp) / "generated"
        subprocess.run([str(BOOTSTRAP), str(target)], check=True)
        result = subprocess.run(
            [str(target / "scripts/verify-structural.sh")],
            cwd=target,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("STRUCTURAL VERIFICATION PASSED", result.stdout)
        self.assertEqual(
            self.tree_hash(target / "packages/contracts"),
            self.tree_hash(PRODUCT / "packages/contracts"),
        )
```

- [x] **Step 2: Run and verify RED**

```bash
cd /Users/jacob/Projects/PCA
python3 -m unittest scripts.tests.test_pack_verification -v
```

Expected: fail because the pack template does not yet contain the completed S0 baseline.

- [x] **Step 3: Synchronize only S0 scaffold files back into the pack**

Copy the verified repository skeleton into `repo-template` while excluding:

```text
.git/
target/
.build/
node_modules/
.next/
*.db
*.log
```

Keep the pack's authoritative docs/tasks/contracts/prompts at their existing roots. Update `verify-pack.sh` so structural mode prints only `STRUCTURAL VERIFICATION PASSED`; full pack validation delegates to a clean bootstrapped target and its `verify-full.sh`.

- [x] **Step 4: Run the complete pack and clean-generation evidence suite**

```bash
cd /Users/jacob/Projects/PCA
python3 -m unittest discover -s scripts/tests -v
./scripts/verify-pack.sh --structural
tmp_target=$(mktemp -d /tmp/pca-final.XXXXXX)/PersonalComputerAgent
./scripts/bootstrap-repo.sh "$tmp_target"
"$tmp_target/scripts/verify-full.sh"
```

Expected: every test passes; structural marker appears only in structural mode; the generated clean repository prints `FULL VERIFICATION PASSED` after all compilers/tests/gates pass.

- [x] **Step 5: Run final product-repository verification and record exact evidence**

```bash
cd /Users/jacob/Projects/PersonalComputerAgent
git status --short
./scripts/verify-full.sh
git log --oneline --decorate -8
```

Expected: worktree is clean; full verification exits 0; Git history contains the S0 commits from Tasks 1-6.

- [x] **Step 6: Commit the final evidence document in the product repository**

Update the copied plan checkboxes and append the exact command exit codes under `## Verification Evidence`, then run:

```bash
git add docs/superpowers/plans/2026-07-30-s0-engineering-baseline.md
git commit -m "docs: record S0 verification evidence"
```

The development pack remains uncommitted because it is not a Git repository; report its changed-file list and hashes explicitly in the final handoff.

---

## Verification Evidence

Recorded on 2026-07-31 on macOS.

### Approved Swift verification deviation

The installed Command Line Tools expose Swift Testing binaries but `swift test` only built the target and returned success without executing its assertions. A mutation from expected `protocol_version == 1` to `== 2` still returned zero, proving the apparent pass was false. The approved option was therefore implemented as the framework-free `BridgeContractVerifier` executable. With the same mutation, `swift run --package-path platform/macos BridgeContractVerifier` exited 1 and printed `expected protocol_version 2`; after restoration it exited 0 and printed `Swift Bridge contract fixture passed`. The full gate and CI invoke this executable directly.

### Toolchains

```text
rustc 1.97.1
cargo 1.97.1
Apple Swift 6.3.3
pnpm 9.15.0
Node v24.18.0
Python 3.9.6
```

### Product repository

- `python3 -m unittest scripts.tests.test_verification_modes -v`: exit 0, 2 tests passed; structural mode never printed the full marker and hidden Cargo failed explicitly.
- `./scripts/verify-structural.sh`: exit 0, 8 schemas validated, migration replay/boundary/failure-path tests passed, one `STRUCTURAL VERIFICATION PASSED` marker.
- `./scripts/verify-full.sh`: exit 0; Rust fmt/clippy/tests, Swift build and executable fixture verifier, pnpm frozen install/typecheck/tests, migration replay, and dependency boundaries passed; one `FULL VERIFICATION PASSED` marker.
- Contract evidence: TypeScript 6 tests passed; Rust shared-fixture integration 2 tests passed; Swift shared-fixture verifier passed; registry covers 16 Appendix C enum groups and 57 Appendix D error codes.
- Migration evidence: local and cloud `0000_baseline.sql` checksums were recorded; the SQLite baseline replayed twice with stable schema plus successful `integrity_check` and `foreign_key_check`.
- Git commits: `022aa25`, `3cec5c3`, `1347cb0`, `6f8d5e2`, `68b1b52`, and `70086cb`.

### Development pack and clean regeneration

- `python3 -m unittest scripts.tests.test_pack_verification -v`: exit 0; regenerated `packages/contracts` matched the product repository after generated dependency folders were excluded.
- `/Users/jacob/Projects/PCA/scripts/verify-pack.sh`: exit 0; 8 schemas and all 3 pack/bootstrap tests passed.
- A clean bootstrap at `/tmp/pca-s0-generated.RmxFTN/PersonalComputerAgent` ran `./scripts/verify-full.sh`: exit 0 with `FULL VERIFICATION PASSED` after fresh Rust, Swift, and pnpm builds.
- Sync exclusions were enforced for `.git`, `target`, `.build`, `node_modules`, `.next`, `.db`, `.log`, and `.DS_Store` artifacts.

### Scope and security review

S0 adds contracts, language mappings, migration metadata, dependency gates, verification, and CI only. It adds no Collector behavior, WeChat access, cloud deployment, resident S1 runtime, secret handling, `.env` changes, or user-data access.

---

## Plan Self-Review

- Spec coverage: bootstrap, dual-directory ownership, canonical schemas, language mappings, fixtures, registries, migrations, boundaries, CI, verification modes, clean generation, and exclusions all map to Tasks 1-7.
- Placeholder scan: no TBD, TODO, “implement later,” or undefined implementation instruction remains.
- Type consistency: all languages use `protocol_version`, `request_id`, `message_kind`, `deadline_ms`, and object `payload` on the wire; Swift property names map through explicit `CodingKeys`.
- Scope: the plan stops at S0 and does not introduce runtime collectors, WeChat, cloud deployment, Dashboard features, or S1 services.

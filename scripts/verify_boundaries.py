#!/usr/bin/env python3
from __future__ import annotations

import re
import sys
from pathlib import Path


RUST_USE = re.compile(r"\b(?:use|extern\s+crate)\s+([A-Za-z0-9_]+)")
TS_IMPORT = re.compile(r"(?:from\s+|import\s*\()(?P<quote>['\"])([^'\"]+)(?P=quote)")
DEPENDENCY_SECTION = re.compile(
    r"^\[(?:target\.[^.]+\.)?(?:dev-|build-)?dependencies(?:\.[^]]+)?\]$"
)
PATH_VALUE = re.compile(r"\bpath\s*=\s*['\"]([^'\"]+)['\"]")


def fail(violations: list[str]) -> int:
    for violation in violations:
        print(violation, file=sys.stderr)
    return 1


def path_dependency_targets(crate_root: Path) -> set[str]:
    manifest_path = crate_root / "Cargo.toml"
    if not manifest_path.is_file():
        return set()
    targets: set[str] = set()
    in_dependency_section = False
    for raw_line in manifest_path.read_text(encoding="utf-8").splitlines():
        line = raw_line.split("#", 1)[0].strip()
        if line.startswith("["):
            in_dependency_section = DEPENDENCY_SECTION.fullmatch(line) is not None
        if in_dependency_section and (match := PATH_VALUE.search(line)):
            targets.add((crate_root / match.group(1)).resolve().as_posix())
    return targets


def rust_symbols(crate_root: Path) -> set[str]:
    symbols: set[str] = set()
    for source in (crate_root / "src").rglob("*.rs") if (crate_root / "src").is_dir() else []:
        symbols.update(RUST_USE.findall(source.read_text(encoding="utf-8")))
    return symbols


def typescript_imports(root: Path) -> set[str]:
    imports: set[str] = set()
    for extension in ("*.ts", "*.tsx", "*.js", "*.jsx"):
        for source in root.rglob(extension) if root.is_dir() else []:
            text = source.read_text(encoding="utf-8")
            imports.update(match.group(2) for match in TS_IMPORT.finditer(text))
    return imports


def main() -> int:
    repository_root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    violations: list[str] = []

    domain = repository_root / "crates/domain"
    domain_targets = path_dependency_targets(domain)
    domain_symbols = rust_symbols(domain)
    if any("/platform/" in target or "/infrastructure/" in target for target in domain_targets) or any(
        symbol.startswith(("pca_platform", "pca_infrastructure")) for symbol in domain_symbols
    ):
        violations.append("forbidden dependency: crates/domain -> platform/infrastructure")

    for collector in (repository_root / "crates").glob("*collector*"):
        targets = path_dependency_targets(collector)
        symbols = rust_symbols(collector)
        if any("cloud-client" in target for target in targets) or any(
            symbol.startswith("pca_cloud_client") for symbol in symbols
        ):
            violations.append(f"forbidden dependency: {collector.relative_to(repository_root)} -> cloud-client")

    web_root = repository_root / "apps/web-dashboard"
    web_imports = typescript_imports(web_root)
    if any(imported == "@pca/db-cloud" or imported.startswith(("@pca/db-cloud/", "drizzle-orm")) for imported in web_imports):
        violations.append("forbidden dependency: apps/web-dashboard -> db-cloud")

    return fail(violations) if violations else (print("Dependency boundaries passed") or 0)


if __name__ == "__main__":
    raise SystemExit(main())

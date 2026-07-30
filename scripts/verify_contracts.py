from pathlib import Path
import json

root = Path(__file__).resolve().parents[1] / "packages" / "contracts"
files = sorted(root.glob("*.schema.json"))
if not files:
    raise SystemExit("No contract schemas found")
for path in files:
    with path.open(encoding="utf-8") as f:
        value = json.load(f)
    if "$schema" not in value or "title" not in value:
        raise SystemExit(f"Invalid schema metadata: {path}")
print(f"Validated {len(files)} contract schemas")

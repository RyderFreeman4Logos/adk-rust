#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

just cargo metadata --manifest-path "$ROOT/Cargo.toml" --no-deps --format-version 1 \
  | python3 -c '
import json
import sys
from collections import defaultdict

metadata = json.load(sys.stdin)
examples = defaultdict(list)

for package in metadata["packages"]:
    for target in package["targets"]:
        if "example" in target["kind"]:
            examples[target["name"]].append((package["name"], target["src_path"]))

duplicates = {
    name: targets
    for name, targets in sorted(examples.items())
    if len(targets) > 1
}

if duplicates:
    print("duplicate example target names found:", file=sys.stderr)
    for name, targets in duplicates.items():
        print(f"  {name}", file=sys.stderr)
        for package, path in targets:
            print(f"    {package}: {path}", file=sys.stderr)
    sys.exit(1)
'

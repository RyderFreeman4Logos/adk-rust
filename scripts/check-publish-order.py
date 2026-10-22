#!/usr/bin/env python3
"""CI check: a valid publish order must exist for all publishable crates.

The publish order itself is computed at runtime by `cargo xtask publish`
(from cargo metadata), so there is no hand-maintained tier list to validate.
What can still go wrong — and what this guards — is the graph itself:

  1. A dependency cycle among publishable crates (normal/build deps).
  2. A *versioned* dev-dependency cycle: `cargo publish` resolves dev-deps
     with version requirements when generating the package lockfile, so a
     versioned dev-dep pointing at a crate that can only be published later
     deadlocks a sequential publish (this broke the v1.0.0 release).
     Path-only dev-deps are stripped at publish and are exempt — keep
     internal dev-deps path-only.
"""

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    meta = json.loads(subprocess.run(
        ["just", "--justfile", str(ROOT / "justfile"), "--working-directory", str(ROOT), "cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout)

    packages = meta["packages"]
    publishable = {
        p["name"] for p in packages
        if not (isinstance(p.get("publish"), list) and not p["publish"])
    }

    deps: dict[str, set[str]] = {}
    versioned_dev: list[str] = []
    for p in packages:
        name = p["name"]
        if name not in publishable:
            continue
        edges = set()
        for d in p["dependencies"]:
            if d["name"] not in publishable:
                continue
            kind = d["kind"] or "normal"
            if kind in ("normal", "build"):
                edges.add(d["name"])
            elif kind == "dev" and d["req"] != "*":
                edges.add(d["name"])
                versioned_dev.append(
                    f"{name} dev-depends on {d['name']} {d['req']} — prefer a "
                    f"path-only dev-dep (no version) so publish order can't deadlock"
                )
        deps[name] = edges

    # Kahn's algorithm: every crate must become publishable eventually.
    remaining = dict(deps)
    rounds = 0
    while remaining:
        ready = [n for n, ds in remaining.items()
                 if all(d not in remaining for d in ds)]
        if not ready:
            print("check-publish-order: no valid publish order exists — "
                  f"cycle among: {sorted(remaining)}", file=sys.stderr)
            for w in versioned_dev:
                print(f"  note: {w}", file=sys.stderr)
            sys.exit(1)
        for n in ready:
            remaining.pop(n)
        rounds += 1

    if versioned_dev:
        print("check-publish-order: order exists, but versioned internal "
              "dev-deps constrain it:", file=sys.stderr)
        for w in versioned_dev:
            print(f"  warning: {w}", file=sys.stderr)

    print(f"check-publish-order: OK ({len(deps)} publishable crates, "
          f"{rounds} dependency tiers, order computable)")


if __name__ == "__main__":
    main()

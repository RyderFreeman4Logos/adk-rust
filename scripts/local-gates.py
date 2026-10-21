#!/usr/bin/env python3
"""Bind a completed Linux gate and independent review to one outgoing branch."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

__all__: list[str] = []


def _git(*args: str) -> str:
    return subprocess.check_output(["git", *args], text=True).strip()


def _snapshot() -> dict[str, str]:
    if _git("status", "--porcelain=v1", "--untracked-files=all"):
        raise ValueError("gate requires a clean index and worktree")
    branch = _git("symbolic-ref", "--quiet", "--short", "HEAD")
    if branch in {"main", "dev"}:
        raise ValueError("create a feature branch before running gates")
    return {"branch": branch, "head": _git("rev-parse", "HEAD"),
            "tree": _git("rev-parse", "HEAD^{tree}"),
            "base": _git("rev-parse", "origin/main^{commit}")}


def _directory() -> Path:
    root = _git("rev-parse", "--show-toplevel")
    key = hashlib.sha256(root.encode()).hexdigest()[:16]
    parent = Path(os.environ["TMPDIR"]) / "adk-rust-local-gates"
    dest = parent / key
    for path in (parent, dest):
        if path.is_symlink():
            raise ValueError("receipt directories must not be symlinks")
        path.mkdir(mode=0o700, exist_ok=True)
        if path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o077:
            raise ValueError("receipt directories must be user-owned mode 0700")
    return dest


def _unique_fields(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate receipt field")
        result[key] = value
    return result


def _read_receipt(path: Path) -> dict[str, object]:
    if path.is_symlink() or not path.is_file() or path.stat().st_uid != os.getuid():
        raise ValueError(f"missing or unsafe receipt: {path}")
    if path.stat().st_size > 8192:
        raise ValueError("receipt exceeds schema budget")
    data = json.loads(path.read_text(), object_pairs_hook=_unique_fields)
    if not isinstance(data, dict):
        raise ValueError("receipt must be a JSON object")
    return data


def _quality(state: dict[str, str]) -> dict[str, str]:
    return {"schema": "adk-rust-quality-v1", "status": "PASS", **state,
            "command": "just _quality-gates"}


def _main() -> None:
    operation = sys.argv[1]
    if operation == "check-branch":
        if _git("symbolic-ref", "--quiet", "--short", "HEAD") in {"main", "dev"}:
            raise ValueError("create a feature branch before committing")
        return
    if operation == "receipt-dir":
        print(_directory())
        return
    before = _snapshot()
    dest = _directory()
    receipt = dest / "quality.json"
    if operation == "produce":
        receipt.unlink(missing_ok=True)
        subprocess.run(["just", "_quality-gates"], check=True)
        if _snapshot() != before:
            raise ValueError("candidate changed during gate")
        with tempfile.NamedTemporaryFile(mode="w", dir=dest, delete=False) as output:
            json.dump(_quality(before), output, sort_keys=True)
            temporary = Path(output.name)
        temporary.replace(receipt)
        print(f"PASS quality receipt: {receipt}")
        return
    if operation != "pre-push":
        raise ValueError("expected check-branch, receipt-dir, produce or pre-push")
    records = sys.stdin.read(4097)
    expected_ref = f"refs/heads/{before['branch']}"
    parts = records.split()
    if (len(records) > 4096 or records.count("\n") != 1 or len(parts) != 4
            or parts[0] != expected_ref or parts[2] != expected_ref
            or parts[1] != before["head"] or not re.fullmatch(r"[0-9a-f]{40}", parts[3])):
        raise ValueError("pre-push requires exactly the checked-out feature branch and HEAD")
    if _read_receipt(receipt) != _quality(before):
        raise ValueError("quality receipt is stale; run just quality-gates")
    review = _read_receipt(dest / "review.json")
    expected = {"schema": "adk-rust-review-v1", "status": "PASS", "head": before["head"],
                "tree": before["tree"], "base": before["base"], "range": "origin/main...HEAD"}
    if set(review) != set(expected) | {"report_path", "report_sha256"} or any(review[k] != v for k, v in expected.items()):
        raise ValueError("independent review receipt is malformed or stale")
    if not isinstance(review["report_path"], str):
        raise ValueError("independent review report path must be a string")
    report = Path(review["report_path"])
    digest = review["report_sha256"]
    if (not report.is_absolute() or report.is_symlink() or not report.is_file()
            or report.stat().st_uid != os.getuid() or not isinstance(digest, str)
            or not re.fullmatch(r"[0-9a-f]{64}", digest)):
        raise ValueError("independent review report is unsafe")
    data = report.read_bytes()
    lines = data.splitlines()
    if (hashlib.sha256(data).hexdigest() != digest or not lines
            or lines[0] != b"VERDICT: PASS" or lines[-1] != b"VERDICT: PASS"):
        raise ValueError("independent review report does not attest PASS")
    if _snapshot() != before:
        raise ValueError("candidate changed while checking receipts")
    print(f"PASS quality and review receipts for {before['head']}")


if __name__ == "__main__":
    try:
        _main()
    except (ValueError, OSError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)

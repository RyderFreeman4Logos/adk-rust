#!/usr/bin/env python3
"""Exercise local receipt custody and the resolved Git hook contract."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

__all__: list[str] = []

ROOT = Path(__file__).resolve().parent.parent


def _run(*args: str, cwd: Path = ROOT, data: str | None = None,
         ok: bool = True, extra_env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    for key in subprocess.check_output(["git", "rev-parse", "--local-env-vars"], cwd=ROOT, text=True).split():
        env.pop(key, None)
    env.update(extra_env or {})
    result = subprocess.run(args, cwd=cwd, env=env, input=data, text=True, capture_output=True)
    if ok and result.returncode:
        raise AssertionError(f"{args}: {result.returncode}\n{result.stdout}{result.stderr}")
    return result


class LocalGatesTests(unittest.TestCase):
    def test_hook_contract(self) -> None:
        dump = _run("lefthook", "dump").stdout
        self.assertIn("piped: true", dump)
        self.assertIn("run: just check-branch", dump)
        self.assertIn("run: just pre-commit-fast", dump)
        self.assertIn("run: just pre-push", dump)
        self.assertIn("use_stdin: true", dump)
        self.assertNotIn("run: cargo", dump)

    def test_upstream_script_tests_remain_in_canonical_coverage(self) -> None:
        commands = _run("just", "--dry-run", "documentation-contracts")
        self.assertIn("python3 -m unittest discover -s scripts/tests", commands.stderr)

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="adk-gates-", dir=os.environ["TMPDIR"])
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)

    def fixture(self) -> None:
        _run("git", "init", "-q", "-b", "main", cwd=self.repo)
        (self.repo / "scripts").mkdir()
        (self.repo / ".local-gates").mkdir()
        shutil.copy(ROOT / "scripts/local-gates.py", self.repo / "scripts/local-gates.py")
        (self.repo / ".gitignore").write_text(".local-gates/\n")
        (self.repo / "justfile").write_text('_quality-gates:\n    @printf run >> .local-gates/runs\n')
        (self.repo / "tracked").write_text("seed\n")
        _run("git", "add", ".", cwd=self.repo)
        _run("git", "-c", "user.name=Gate Test", "-c", "user.email=gate@example.invalid", "commit", "-qm", "seed", cwd=self.repo)
        _run("git", "update-ref", "refs/remotes/origin/main", "HEAD", cwd=self.repo)
        _run("git", "switch", "-c", "chore/test", cwd=self.repo)
        self.receipts = Path(self.gate("receipt-dir").stdout.strip())
        self.addCleanup(shutil.rmtree, self.receipts)
        self.head = _run("git", "rev-parse", "HEAD", cwd=self.repo).stdout.strip()
        self.tree = _run("git", "rev-parse", "HEAD^{tree}", cwd=self.repo).stdout.strip()

    def gate(self, operation: str, data: str | None = None, ok: bool = True) -> subprocess.CompletedProcess[str]:
        return _run("python3", "scripts/local-gates.py", operation, cwd=self.repo, data=data, ok=ok)

    def review(self) -> None:
        report = self.receipts / "report.md"
        report.write_text("VERDICT: PASS\nReview fixture.\nVERDICT: PASS\n")
        receipt = {"schema": "adk-rust-review-v1", "status": "PASS", "head": self.head,
                   "tree": self.tree, "base": self.head, "range": "origin/main...HEAD",
                   "report_path": str(report), "report_sha256": hashlib.sha256(report.read_bytes()).hexdigest()}
        (self.receipts / "review.json").write_text(json.dumps(receipt))

    def cargo_spy(self) -> dict[str, str]:
        binary = self.repo / "bin"
        binary.mkdir()
        (binary / "rustup").write_text("#!/bin/bash\nexit 1\n")
        (binary / "cargo").write_text('#!/bin/bash\nif [[ "$1" == --version ]]; then echo "cargo 1.95.0"; else printf "args=%s\\n" "$*"; printf "io=%s\\n" "$(ionice -p $$)"; printf "tmp=%s\\n" "$TMPDIR"; printf "rustdocflags=%s\\n" "${RUSTDOCFLAGS-}"; fi\n')
        for path in binary.iterdir():
            path.chmod(0o700)
        return {"PATH": str(binary) + os.pathsep + os.environ["PATH"]}

    def test_nested_launcher_target_remains_ignored(self) -> None:
        manifest = sorted(ROOT.glob("examples/*/Cargo.toml"))[0]
        target = manifest.parent / "target"
        expected = "/ssd/mirror-rootfs" + str(target)
        _run("just", "cargo", "metadata", "--manifest-path", str(manifest), extra_env=self.cargo_spy())
        self.assertEqual(os.readlink(target), expected)
        self.assertEqual(_run("git", "check-ignore", str(target), ok=False).returncode, 0)

    def test_docs_command_denies_rustdoc_warnings(self) -> None:
        result = _run("just", "docs", extra_env=self.cargo_spy())
        self.assertIn("args=doc --workspace --no-deps --locked\n", result.stdout)
        self.assertIn("rustdocflags=-D warnings\n", result.stdout)

    def test_cargo_launcher_preserves_pin_priority_and_target(self) -> None:
        env = self.cargo_spy()
        original = os.readlink(ROOT / "target")
        result = _run("just", "cargo", "metadata", "--locked", extra_env=env)
        self.assertIn("args=metadata --locked", result.stdout)
        self.assertIn("io=idle", result.stdout)
        self.assertIn("tmp=" + str((Path.home() / "tmp").resolve()), result.stdout)
        self.assertEqual(os.readlink(ROOT / "target"), original)
        for flag in ("--target-dir=forbidden", "--jobs=8", "--config=build.jobs=8", "+stable"):
            self.assertNotEqual(_run("just", "cargo", flag, ok=False, extra_env=env).returncode, 0)
        for key in ("CARGO_TARGET_DIR", "CARGO_HOME", "CARGO_BUILD_JOBS"):
            self.assertNotEqual(_run("just", "cargo", "metadata", ok=False, extra_env={**env, key: "forbidden"}).returncode, 0)

    def test_target_guard_preserves_unexpected_targets(self) -> None:
        command = 'ADK_GATE_ROOT="$1"; source "$1/scripts/local-env.sh"; adk_prepare_target "$2"'
        target = self.repo / "target"
        target.mkdir()
        self.assertNotEqual(_run("bash", "-c", command, "guard", str(ROOT), str(self.repo), ok=False).returncode, 0)
        self.assertTrue(target.is_dir())
        target.rmdir()
        target.symlink_to("/unexpected-adk-target")
        self.assertNotEqual(_run("bash", "-c", command, "guard", str(ROOT), str(self.repo), ok=False).returncode, 0)
        self.assertEqual(os.readlink(target), "/unexpected-adk-target")

    def test_receipts_bind_exact_clean_head_and_outgoing_ref(self) -> None:
        self.fixture()
        self.gate("produce")
        update = f"refs/heads/chore/test {self.head} refs/heads/chore/test {'0' * 40}\n"
        self.assertNotEqual(self.gate("pre-push", update, ok=False).returncode, 0)
        self.review()
        self.gate("pre-push", update)
        self.assertEqual((self.repo / ".local-gates/runs").read_text(), "run")
        for bad in ("", "broken\n", update * 2,
                    update.replace("refs/heads/chore/test", "refs/heads/main"),
                    update.replace(self.head, "0" * 40),
                    update.replace("refs/heads/chore/test", "refs/tags/v1"),
                    update.replace(self.head, "1" * 40)):
            self.assertNotEqual(self.gate("pre-push", bad, ok=False).returncode, 0, bad)
        (self.repo / "tracked").write_text("dirty\n")
        self.assertNotEqual(self.gate("pre-push", update, ok=False).returncode, 0)
        _run("git", "restore", "tracked", cwd=self.repo)
        receipt = self.receipts / "review.json"
        good = receipt.read_text()
        for bad in ("{}", good.replace(self.head, "1" * 40), good[:-1] + ',"extra":1}'):
            receipt.write_text(bad)
            self.assertNotEqual(self.gate("pre-push", update, ok=False).returncode, 0)
        receipt.write_text(good)
        (self.receipts / "report.md").write_text("VERDICT: FAIL\n")
        self.assertNotEqual(self.gate("pre-push", update, ok=False).returncode, 0)

    def test_failed_or_mutating_gate_never_leaves_pass_receipt(self) -> None:
        self.fixture()
        for command in ("false", "printf dirty >> tracked"):
            (self.repo / "justfile").write_text(f"_quality-gates:\n    @{command}\n")
            _run("git", "add", "justfile", cwd=self.repo)
            _run("git", "-c", "user.name=Gate Test", "-c", "user.email=gate@example.invalid", "commit", "-qm", "gate fixture", cwd=self.repo)
            self.assertNotEqual(self.gate("produce", ok=False).returncode, 0)
            self.assertFalse((self.receipts / "quality.json").exists())
            _run("git", "restore", "tracked", cwd=self.repo)

    def test_installed_pre_push_blocks_transport_without_review(self) -> None:
        self.fixture()
        remote = self.repo / ".local-gates/remote.git"
        _run("git", "init", "--bare", "-q", str(remote))
        _run("git", "remote", "add", "transport", str(remote), cwd=self.repo)
        shutil.copy(ROOT / "lefthook.yml", self.repo / "lefthook.yml")
        with (self.repo / "justfile").open("a") as output:
            output.write("pre-push:\n    @python3 scripts/local-gates.py pre-push\n")
        _run("git", "add", ".", cwd=self.repo)
        _run("git", "-c", "user.name=Gate Test", "-c", "user.email=gate@example.invalid", "commit", "-qm", "hook fixture", cwd=self.repo)
        self.head = _run("git", "rev-parse", "HEAD", cwd=self.repo).stdout.strip()
        self.tree = _run("git", "rev-parse", "HEAD^{tree}", cwd=self.repo).stdout.strip()
        _run("git", "update-ref", "refs/remotes/origin/main", "HEAD", cwd=self.repo)
        _run("lefthook", "install", cwd=self.repo)
        self.gate("produce")
        self.assertNotEqual(_run("git", "push", "transport", "chore/test", cwd=self.repo, ok=False).returncode, 0)
        self.assertNotEqual(_run("git", "--git-dir", str(remote), "show-ref", "--verify", "refs/heads/chore/test", ok=False).returncode, 0)
        self.review()
        _run("git", "push", "transport", "chore/test", cwd=self.repo)
        self.assertIn(self.head, _run("git", "--git-dir", str(remote), "show-ref", "--verify", "refs/heads/chore/test").stdout)
        self.assertEqual((self.repo / ".local-gates/runs").read_text(), "run")

    def test_branch_guard_stops_expensive_hook(self) -> None:
        self.fixture()
        shutil.copy(ROOT / "lefthook.yml", self.repo / "lefthook.yml")
        (self.repo / "justfile").write_text('check-branch:\n    @printf guard >> .local-gates/hook\n    @false\npre-commit-fast:\n    @printf expensive >> .local-gates/hook\n')
        (self.repo / ".local-gates").mkdir(exist_ok=True)
        _run("git", "add", ".", cwd=self.repo)
        self.assertNotEqual(_run("lefthook", "run", "pre-commit", cwd=self.repo, ok=False).returncode, 0)
        self.assertEqual((self.repo / ".local-gates/hook").read_text(), "guard")


if __name__ == "__main__":
    unittest.main()

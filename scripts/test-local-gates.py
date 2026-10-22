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

    def cargo_spy(self, rustup: bool = True) -> dict[str, str]:
        binary = self.repo / "bin"
        binary.mkdir()
        if rustup:
            (binary / "rustup").write_text('''#!/bin/bash
if [[ "$*" == "which --toolchain 1.95.0 cargo" ]]; then
    printf '%s/cargo\\n' "$(dirname "$0")"
else
    exit 1
fi
''')
        else:
            for name in ("just", "bash", "python3", "readlink", "sed", "cut", "head", "awk", "dirname", "ionice"):
                (binary / name).symlink_to(shutil.which(name))
        (binary / "cargo").write_text("""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys
if sys.argv[1:] == ["--version"]:
    print("cargo " + os.environ.get("ADK_SPY_VERSION", "1.95.0"))
    sys.exit(0)
print("args=" + " ".join(sys.argv[1:]))
print("io=" + subprocess.check_output(["ionice", "-p", str(os.getpid())], text=True).strip())
print("tmp=" + os.environ["TMPDIR"])
print("rustdocflags=" + os.environ.get("RUSTDOCFLAGS", ""))
if "ADK_SPY_LOG" in os.environ:
    with open(os.environ["ADK_SPY_LOG"], "a") as output:
        output.write(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd(),
                                 "target_env": os.environ.get("CARGO_TARGET_DIR")}) + "\\n")
if "adk" in sys.argv and "new" in sys.argv:
    project = Path(sys.argv[sys.argv.index("new") + 1])
    project.mkdir()
    (project / "Cargo.toml").write_text('[package]\\nname = "fixture"\\nversion = "0.1.0"\\nedition = "2024"\\n')
""")
        for path in binary.iterdir():
            if not path.is_symlink():
                path.chmod(0o700)
        return {"PATH": str(binary) + (os.pathsep + os.environ["PATH"] if rustup else "")}

    def test_pin_resolution_does_not_depend_on_toolchain_listing(self) -> None:
        env = {**self.cargo_spy(), "ADK_SPY_LOG": str(self.repo / "calls.jsonl")}
        binary = self.repo / "bin"
        pinned = self.repo / "pinned"
        pinned.mkdir()
        shutil.copy(binary / "cargo", pinned / "cargo")
        with (pinned / "cargo").open("a") as output:
            output.write('print(subprocess.check_output(["rustc", "--version"], text=True).strip())\n'
                         'print(subprocess.check_output(["cargo-clippy", "--version"], text=True).strip())\n'
                         'sys.exit(47)\n')
        for name in ("rustc", "cargo-clippy"):
            (pinned / name).write_text(f"#!/bin/bash\nprintf 'selected {name} 1.95.0\\n'\n")
            (pinned / name).chmod(0o700)
        (binary / "cargo").write_text("#!/bin/bash\necho 'cargo 1.99.1'\nexit 89\n")
        (binary / "rustup").write_text(f'''#!/bin/bash
if [[ "$*" == "which --toolchain 1.95.0 cargo" ]]; then
    printf '%s\\n' '{pinned / "cargo"}'
else
    echo 'toolchain listing deliberately unavailable' >&2
    exit 23
fi
''')
        arguments = ["metadata", "--locked", "--", "argument with spaces", ""]
        result = _run("just", "cargo", *arguments, ok=False, extra_env=env)
        self.assertEqual(result.returncode, 47, result.stdout + result.stderr)
        self.assertIn("selected rustc 1.95.0", result.stdout)
        self.assertIn("selected cargo-clippy 1.95.0", result.stdout)
        records = [json.loads(line) for line in (self.repo / "calls.jsonl").read_text().splitlines()]
        self.assertEqual([call["args"] for call in records], [arguments])

    def test_failed_pin_resolution_never_executes_path_cargo(self) -> None:
        env = {**self.cargo_spy(), "ADK_SPY_LOG": str(self.repo / "calls.jsonl")}
        (self.repo / "bin/rustup").write_text("#!/bin/bash\necho 'pin resolution failed: missing toolchain' >&2\nexit 23\n")
        result = _run("just", "cargo", "metadata", ok=False, extra_env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pin resolution failed: missing toolchain", result.stderr)
        self.assertFalse((self.repo / "calls.jsonl").exists())

    def test_native_exact_version_path_without_rustup(self) -> None:
        env = {**self.cargo_spy(rustup=False), "ADK_SPY_LOG": str(self.repo / "calls.jsonl")}
        self.assertIsNone(shutil.which("rustup", path=env["PATH"]))
        result = _run("just", "cargo", "metadata", "--locked", extra_env=env)
        self.assertIn("args=metadata --locked", result.stdout)
        calls = (self.repo / "calls.jsonl").read_bytes()
        result = _run("just", "cargo", "metadata", ok=False,
                      extra_env={**env, "ADK_SPY_VERSION": "1.99.1"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolved cargo 1.99.1, pin is 1.95.0", result.stderr)
        self.assertEqual(calls, (self.repo / "calls.jsonl").read_bytes())

    def test_example_failures_retain_full_diagnostics_and_continue(self) -> None:
        (self.repo / "scripts").mkdir()
        for name in ("check-examples-compile.sh", "local-env.sh"):
            shutil.copy(ROOT / "scripts" / name, self.repo / "scripts" / name)
        for name in ("a_failure", "z_success"):
            project = self.repo / "examples" / name
            project.mkdir(parents=True)
            (project / "Cargo.toml").write_text("[package]\n")
        binary = self.repo / "bin"
        binary.mkdir()
        launcher = binary / "just"
        launcher.write_text("""#!/usr/bin/env python3
import os
import sys
if "examples/a_failure/Cargo.toml" in sys.argv:
    print("stderr_file=" + os.readlink("/proc/self/fd/2"))
    for line in range(31):
        print("build-script preamble " + str(line), file=sys.stderr)
    print("ADK_NATIVE_CAUSE_AFTER_PREAMBLE_31", file=sys.stderr)
    sys.exit(101)
""")
        launcher.chmod(0o700)
        result = _run("bash", str(self.repo / "scripts/check-examples-compile.sh"),
                      cwd=self.repo, ok=False,
                      extra_env={"PATH": str(binary) + os.pathsep + os.environ["PATH"]})
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("ok       examples/z_success", result.stdout)
        self.assertIn("examples checked: 2, skipped: 0, failed: 1", result.stdout)
        stderr_file = next(line.removeprefix("stderr_file=") for line in result.stdout.splitlines()
                           if line.startswith("stderr_file="))
        self.assertFalse(Path(stderr_file).exists(), "collector must clean its temporary stderr")
        self.assertIn("ADK_NATIVE_CAUSE_AFTER_PREAMBLE_31", result.stdout)

    def test_generated_templates_use_existing_sdk_target(self) -> None:
        env = {**self.cargo_spy(), "ADK_SPY_LOG": str(self.repo / "calls.jsonl")}
        before = (os.readlink(ROOT / "target"), (ROOT / "target").lstat().st_ino,
                  (ROOT / "target").stat().st_ino)
        result = _run("bash", "scripts/check-cargo-adk-templates.sh", ok=False, extra_env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        records = [json.loads(line) for line in (self.repo / "calls.jsonl").read_text().splitlines()]
        checks = [call for call in records if "check" in call["args"]]
        self.assertEqual(len(checks), 12)
        for call in checks:
            self.assertEqual(call["cwd"], str(ROOT))
            self.assertIsNone(call["target_env"])
            self.assertIn("build.target-dir=" + json.dumps(before[0]), call["args"])
            self.assertIn("build.build-dir=" + json.dumps(before[0]), call["args"])
            manifest = Path(call["args"][call["args"].index("--manifest-path") + 1])
            self.assertTrue(manifest.is_absolute())
            self.assertTrue(manifest.is_relative_to(Path(os.environ["TMPDIR"])))
            self.assertFalse(manifest.parent.exists(), "template cleanup must remove only its source")
        self.assertEqual(before, (os.readlink(ROOT / "target"), (ROOT / "target").lstat().st_ino,
                                  (ROOT / "target").stat().st_ino))

    def test_template_entry_rejects_caller_overrides(self) -> None:
        env = {**self.cargo_spy(), "ADK_SPY_LOG": str(self.repo / "calls.jsonl")}
        manifest = self.repo / "Cargo.toml"
        manifest.write_text('[package]\nname = "fixture"\nversion = "0.1.0"\nedition = "2024"\n')
        result = _run("just", "template-cargo", "metadata", str(manifest), extra_env=env)
        self.assertIn("io=idle", result.stdout)
        self.assertIn("tmp=" + os.environ["TMPDIR"], result.stdout)
        records = (self.repo / "calls.jsonl").read_bytes()
        for flag in ("--target-dir=forbidden", "--jobs=8", "-j8", "--config=build.jobs=8", "+stable", "build", "--manifest-path=forbidden"):
            self.assertNotEqual(_run("just", "template-cargo", flag, str(manifest), ok=False, extra_env=env).returncode, 0)
            self.assertNotEqual(_run("just", "template-cargo", "metadata", str(manifest), flag, ok=False, extra_env=env).returncode, 0)
        for key in ("CARGO_TARGET_DIR", "CARGO_HOME", "CARGO_BUILD_JOBS", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"):
            self.assertNotEqual(_run("just", "template-cargo", "metadata", str(manifest), ok=False,
                                     extra_env={**env, key: "forbidden"}).returncode, 0)
        for bad in (str(ROOT / "Cargo.toml"), "Cargo.toml", str(self.repo / "absent.toml")):
            self.assertNotEqual(_run("just", "template-cargo", "metadata", bad, ok=False, extra_env=env).returncode, 0)
        self.assertNotEqual(_run("just", "template-cargo", "metadata", str(manifest), ok=False,
                                 extra_env={**env, "ADK_SPY_VERSION": "1.96.0"}).returncode, 0)
        self.assertEqual(records, (self.repo / "calls.jsonl").read_bytes())

    def test_real_template_entry_uses_same_canonical_target(self) -> None:
        manifest = self.repo / "Cargo.toml"
        package = "adk_gate_target_probe_" + self.repo.name.replace("-", "_")
        manifest.write_text(f'[package]\nname = "{package}"\nversion = "0.1.0"\nedition = "2024"\n')
        (self.repo / "src").mkdir()
        (self.repo / "src/lib.rs").write_text("pub fn target_probe() -> bool { true }\n")
        before = (os.readlink(ROOT / "target"), (ROOT / "target").lstat().st_ino,
                  (ROOT / "target").stat().st_ino)
        metadata = json.loads(_run("just", "template-cargo", "metadata", str(manifest)).stdout)
        self.assertEqual(metadata["workspace_root"], str(self.repo))
        destination = (ROOT / "target").stat()
        for field in ("target_directory", "build_directory"):
            if field in metadata:
                reported = Path(metadata[field]).stat()
                self.assertEqual((reported.st_dev, reported.st_ino), (destination.st_dev, destination.st_ino))
        self.assertIn("target_directory", metadata)
        _run("just", "template-cargo", "check", str(manifest))
        artifacts = list((ROOT / "target/debug/deps").glob(f"lib{package}-*.rmeta"))
        intermediates = list((ROOT / "target/debug/deps").glob(f"{package}-*.d"))
        self.assertTrue(artifacts)
        self.assertTrue(intermediates)
        self.assertFalse(os.path.lexists(self.repo / "target"))
        print("template Cargo ownership: " + json.dumps({
            "pin": "1.95.0", "root_link": before[0], "root_link_inode": before[1],
            "destination_inode": before[2], "manifest": str(manifest),
            "target_directory": metadata["target_directory"],
            "build_directory": metadata.get("build_directory"),
            "artifacts": [str(path) for path in artifacts],
            "intermediates": [str(path) for path in intermediates], "source_target_exists": False,
        }, sort_keys=True))
        self.assertFalse(Path("/ssd/mirror-rootfs/mnt").exists())
        self.assertEqual(before, (os.readlink(ROOT / "target"), (ROOT / "target").lstat().st_ino,
                                  (ROOT / "target").stat().st_ino))

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

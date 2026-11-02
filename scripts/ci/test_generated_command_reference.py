"""Independent acceptance for the explicit command-reference lifecycle.

Fast, offline policy tests run through normal scripts/ci discovery. For real
filesystem mutation checks, first compile reference_docs and the generator via
the bounded Cargo wrapper on the controller, then run:

  python3 scripts/ci/test_generated_command_reference.py \
    --reference-test-binary /path/to/reference_docs-harness \
    --generator-binary /path/to/generate_command_reference

The runtime suite uses a temporary REFERENCE_DOCS_ROOT, never the checkout's
references, a database, Discord, or any live environment. No runtime test is
silently skipped: supplying both binaries explicitly selects that suite.
"""

import argparse
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
GENERATED = "docs/generated/commands.md"
CONFIGURATION = "docs/configuration.md"
GENERATOR = "scripts/generate_command_reference.py"
ORACLES = (
    "crates/core/tests/fixtures/smoke_surface_inventory.json",
    "crates/core/tests/fixtures/staging_published_commands.json",
    "docs/staging-e2e-command-matrix.md",
)
CHECK = "committed_references_match_compiled_metadata"


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_workflow(path):
    # PyYAML belongs to offline policy checks, not the real-binary runtime suite.
    import yaml

    return yaml.load(path.read_text(), Loader=yaml.BaseLoader)


def logical_lines(script):
    return [line.strip() for line in script.replace("\\\n", " ").splitlines()
            if line.strip() and not line.lstrip().startswith("#")]


def compiles_reference_consumers(line):
    executable = r'(?:\bcargo|"\$COV")\s+'
    if not re.search(executable + r"(?:test|clippy|build|check|nextest\s+run|llvm-cov)\b", line):
        return False
    if re.search(executable + r"llvm-cov\s+(?:clean|report|show-env)\b", line):
        return False
    if re.search(executable + r"llvm-cov\b", line):
        return True  # coverage invokes its standalone executable through COV
    if re.search(r"\bcargo\s+(?:build|check)\b", line) and "--all-targets" not in line:
        return False
    return (("--workspace" in line and "--doc" not in line)
            or re.search(r"(?:-p|--package)\s+two-bot(?:\s|$)", line))


class GeneratedCommandReferencePolicy(unittest.TestCase):
    def test_runtime_entry_needs_only_stdlib(self):
        probe = r'''
import importlib.util
import runpy
import sys
import unittest
from unittest import mock

assert importlib.util.find_spec("yaml") is None, "probe must exclude optional PyYAML"
sys.argv = [sys.argv[1], "--reference-test-binary", "/fixture-reference",
            "--generator-binary", "/fixture-generator"]
# Inspect CLI dispatch without pretending to execute the real Rust binaries.
with mock.patch.object(unittest.TextTestRunner, "run") as run:
    run.return_value.wasSuccessful.return_value = True
    try:
        runpy.run_path(sys.argv[0], run_name="__main__")
    except SystemExit as exit:
        assert exit.code == 0, exit.code
    else:
        raise AssertionError("runtime entry must report its result")
    run.assert_called_once()
    suite = run.call_args.args[0]
    assert suite.countTestCases() == 3
    assert {test._testMethodName for test in suite} == {
        "test_absent_reference_is_rejected_then_generation_is_deterministic",
        "test_stale_and_incorrect_bytes_fail_without_repair",
        "test_configuration_drift_cannot_be_silently_regenerated",
    }
'''
        result = subprocess.run(
            [sys.executable, "-I", "-S", "-c", probe, str(Path(__file__).resolve())],
            capture_output=True, text=True, check=False, timeout=30)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_controller_mode_is_explicit_opt_in_only(self):
        generator = load_script("generate_command_reference")
        cases = (
            ([], {"PAPERCLIP_RUN_ID": "run"}, False),
            ([], {"TWO_CARGO_CONTROLLER": "0"}, False),
            ([], {"TWO_CARGO_CONTROLLER": "1"}, True),
            (["--controller"], {}, True),
        )
        for argv, environment, controller in cases:
            with self.subTest(argv=argv, environment=environment):
                clean = {key: value for key, value in os.environ.items()
                         if key not in ("PAPERCLIP_RUN_ID", "TWO_CARGO_CONTROLLER")}
                with mock.patch.dict(os.environ, clean | environment, clear=True), \
                        mock.patch.object(generator, "build_executable",
                                          return_value=(None, 1)) as build:
                    self.assertEqual(generator.main(argv), 1)
                cargo = build.call_args.args[0]
                self.assertEqual("scripts/cargo_cache.py" in " ".join(cargo), controller)
                self.assertEqual(cargo == ["cargo"], not controller)

    def test_consumer_census_covers_standalone_coverage_and_filtered_bot_tests(self):
        for command in (
            'cov_run "$COV" llvm-cov --workspace --lib --bins --tests',
            'cargo llvm-cov --no-report "${cov_args[@]}"',
            "cargo test -p two-bot --bin two-bot unrelated_filter",
            "cargo clippy --workspace --all-targets --all-features",
            "cargo build --workspace --all-targets",
        ):
            self.assertTrue(compiles_reference_consumers(command), command)
        for command in (
            'cov_run "$COV" llvm-cov report --workspace',
            "cargo llvm-cov clean --workspace",
            "cargo build --release --locked",
            "cargo test -p two-bot-core --test reference_docs",
        ):
            self.assertFalse(compiles_reference_consumers(command), command)

    def test_generated_output_is_ignored_but_authored_contracts_stay_tracked(self):
        tracked = set(subprocess.check_output(
            ["git", "ls-files", "-z"], cwd=ROOT).decode().split("\0"))
        self.assertNotIn(GENERATED, tracked)
        ignored = subprocess.run(["git", "check-ignore", "--no-index", GENERATED],
                                 cwd=ROOT, capture_output=True, text=True, check=False)
        self.assertEqual(ignored.returncode, 0, "ignore the generated output, not its inputs")
        for path in ("docs/commands.md", CONFIGURATION, *ORACLES):
            with self.subTest(path=path):
                self.assertIn(path, tracked)
                self.assertTrue((ROOT / path).is_file())
                self.assertNotEqual(subprocess.run(
                    ["git", "check-ignore", "--no-index", path], cwd=ROOT,
                    capture_output=True, check=False).returncode, 0)
        landing = (ROOT / "docs/commands.md").read_text()
        self.assertIn("generated/commands.md", landing)
        self.assertIn(GENERATOR, landing)
        self.assertNotIn("## `/", landing, "the tracked landing page must not retain registry rows")

    def test_checker_remains_read_only_and_renderer_is_reused(self):
        source = (ROOT / "crates/core/tests/reference_docs.rs").read_text()
        self.assertIn(f'("{GENERATED}", render_commands())', source)
        self.assertIn(f'("{CONFIGURATION}", render_configuration())', source)
        body = source.split(f"fn {CHECK}()", 1)[1].split("#[test]", 1)[0]
        self.assertIn("read_to_string", body)
        self.assertIn("assert_eq!", body)
        for forbidden in ("std::fs::write", "regenerate_reference_docs(", "Command::new"):
            self.assertNotIn(forbidden, body)
        example = ROOT / "crates/core/examples/generate_command_reference.rs"
        self.assertTrue(example.is_file(), "explicit generator is a separate operation")
        self.assertIn('include!("../tests/reference_docs.rs")', example.read_text())

    def test_content_readers_follow_generated_output(self):
        for path in (
            "crates/bot/src/giveaway_create_entry_acceptance_tests.rs",
            "crates/bot/src/voice_setup_access_paging_acceptance_tests.rs",
        ):
            with self.subTest(path=path):
                text = (ROOT / path).read_text()
                self.assertIn(GENERATED, text)
                self.assertNotIn("../../docs/commands.md", text)

    def test_documented_local_verification_generates_before_compiling_tests(self):
        self.assertTrue((ROOT / GENERATOR).is_file())
        for path in ("AGENTS.md", "CONTRIBUTING.md"):
            with self.subTest(path=path):
                text = (ROOT / path).read_text()
                self.assertIn(GENERATOR, text)
                self.assertLess(text.index(GENERATOR), text.index("cargo clippy --workspace"))
                self.assertIn("--controller", text)
        self.assertIn(GENERATOR, (ROOT / "README.md").read_text())

    def test_every_ci_test_compile_has_prior_explicit_generation(self):
        consumers = []
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            workflow = load_workflow(path)
            for job_id, job in workflow.get("jobs", {}).items():
                generators = []
                for step in job.get("steps", []):
                    for line in logical_lines(step.get("run", "")):
                        if GENERATOR in line and not line.startswith(("echo ", "printf ")):
                            generators.append(step)
                        if not compiles_reference_consumers(line):
                            continue
                        label = f"{path.name}:{job_id}:{step.get('name', line)}"
                        consumers.append(label)
                        with self.subTest(consumer=label):
                            eligible = [g for g in generators if
                                        not g.get("continue-on-error")
                                        and (not g.get("if") or g.get("if") == step.get("if"))]
                            self.assertTrue(eligible, "generate in this checkout before its consumers")
        self.assertTrue(consumers, "consumer census must not become vacuous")
        for workflow in ("check.yml", "nightly.yml", "reusable-rust-nextest.yml"):
            self.assertTrue(any(label.startswith(workflow + ":") for label in consumers))

    def test_ci_runs_real_generator_and_read_only_checker_mutation_battery(self):
        workflow = load_workflow(ROOT / ".github/workflows/check.yml")
        direct = [step for job in workflow["jobs"].values() for step in job.get("steps", [])
                  if all(token in step.get("run", "") for token in (
                      "scripts/ci/test_generated_command_reference.py",
                      "--reference-test-binary", "--generator-binary"))]
        wrapped = [step for job in workflow["jobs"].values() for step in job.get("steps", [])
                   if GENERATOR in step.get("run", "")
                   and "--verify-lifecycle" in step.get("run", "")]
        self.assertTrue(direct or wrapped,
                        "offline structural pins alone are not generator execution evidence")
        for step in direct + wrapped:
            self.assertFalse(step.get("continue-on-error"), "mutation failures must reach ci-ok")
        if wrapped:
            module = load_script("generate_command_reference")
            for battery_status in (0, 1):
                with self.subTest(battery_status=battery_status), \
                     mock.patch.object(module, "build_executable", side_effect=[
                         ("/fixture-generator", 0), ("/fixture-harness", 0)]), \
                     mock.patch.object(module.subprocess, "run", side_effect=[
                         subprocess.CompletedProcess([], 0, stdout="{}\n"),
                         subprocess.CompletedProcess([], battery_status)]) as run, \
                     contextlib.redirect_stdout(io.StringIO()):
                    status = module.main(["--verify-lifecycle"])
                    self.assertEqual(status == 0, battery_status == 0)
                    invocation = run.call_args_list[1].args[0]
                    self.assertIn(str(ROOT / "scripts/ci/test_generated_command_reference.py"),
                                  invocation)
                    for flag, executable in (("--reference-test-binary", "/fixture-harness"),
                                             ("--generator-binary", "/fixture-generator")):
                        self.assertEqual(invocation[invocation.index(flag) + 1], executable)

    def test_ci_uploads_inspectable_generated_reference(self):
        workflow = load_workflow(ROOT / ".github/workflows/check.yml")
        uploads = []
        for job in workflow["jobs"].values():
            generated = False
            for step in job.get("steps", []):
                generated |= GENERATOR in step.get("run", "")
                if "actions/upload-artifact@" not in step.get("uses", ""):
                    continue
                settings = step.get("with", {})
                if GENERATED in settings.get("path", ""):
                    self.assertTrue(generated, "upload from a checkout that generated the bytes")
                    self.assertEqual(settings.get("if-no-files-found"), "error")
                    uploads.append(step)
        self.assertTrue(uploads, "generated docs need an inspectable CI artifact")

    def test_markdown_corpus_includes_generated_reference_when_present(self):
        module = load_script("test_docs_path_conformance")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            output = root / GENERATED
            output.parent.mkdir(parents=True)
            output.write_text("# Generated reference\n")
            (root / ".gitignore").write_text(GENERATED + "\n")
            module.ROOT = root
            self.assertIn(output, module._markdown_corpus(),
                          "git ls-files alone silently drops ignored generated references")

    def test_docs_only_index_tolerates_only_known_absent_generated_output(self):
        module = load_script("check_docs_index")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "docs/generated").mkdir(parents=True)
            (root / "docs/commands.md").write_text("# Command reference landing\n")
            index = root / "docs/README.md"
            index.write_text("# Map\n- [Map](README.md)\n- [Commands](commands.md)\n"
                             "- [Generated commands](generated/commands.md)\n")
            report = module.check(root)
            self.assertEqual(report, {"missing": [], "stale": [], "oversize": []})
            output = root / GENERATED
            output.write_bytes(b"x" * (module.DOC_LIMIT + 1))
            self.assertIn(GENERATED, module.check(root)["oversize"])
            index.write_text(index.read_text() + "- [Missing](missing.md)\n")
            self.assertIn("docs/missing.md", module.check(root)["stale"])


def runtime_suite(reference_binary, generator_binary):
    """Create tests only on explicit invocation, using actual compiled binaries."""
    class ReferenceLifecycle(unittest.TestCase):
        def setUp(self):
            self.temporary = tempfile.TemporaryDirectory()
            self.addCleanup(self.temporary.cleanup)
            self.root = Path(self.temporary.name)
            self.environment = dict(os.environ, REFERENCE_DOCS_ROOT=str(self.root))
            self.command = self.root / GENERATED
            self.command.parent.mkdir(parents=True)
            self.configuration = self.root / CONFIGURATION
            self.configuration.write_bytes((ROOT / CONFIGURATION).read_bytes())
            for path in ORACLES:
                target = self.root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes((ROOT / path).read_bytes())
            listing = subprocess.run([reference_binary, "--list"], capture_output=True,
                                     text=True, env=self.environment, check=True)
            self.assertIn(f"{CHECK}: test", listing.stdout, "reject a zero-test harness")

        def snapshot(self):
            return {path: (path.read_bytes(), path.stat().st_mtime_ns)
                    for path in self.root.rglob("*") if path.is_file()}

        def generate(self, success=True):
            before = {p: value for p, value in self.snapshot().items() if p != self.command}
            result = subprocess.run([generator_binary], capture_output=True,
                                    text=True, env=self.environment, check=False)
            self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
            after = {p: value for p, value in self.snapshot().items() if p != self.command}
            self.assertEqual(after, before, "generator may write only the command reference")
            if success:
                self.assertIn("## `/", self.command.read_text())

        def check_reference(self, success):
            before = self.snapshot()
            result = subprocess.run([reference_binary, "--exact", CHECK, "--nocapture"],
                                    capture_output=True, text=True, env=self.environment,
                                    check=False)
            self.assertIn("running 1 test", result.stdout)
            self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
            self.assertEqual(self.snapshot(), before, "comparison must never repair any output")

        def test_absent_reference_is_rejected_then_generation_is_deterministic(self):
            self.check_reference(False)
            self.assertFalse(self.command.exists())
            self.generate()
            first = self.command.read_bytes()
            self.check_reference(True)
            self.command.unlink()
            self.generate()
            self.assertEqual(self.command.read_bytes(), first)
            self.generate()
            self.assertEqual(self.command.read_bytes(), first)
            self.check_reference(True)

        def test_stale_and_incorrect_bytes_fail_without_repair(self):
            self.generate()
            correct = self.command.read_bytes()
            for corrupt in (correct + b"\nStale acceptance sentinel.\n", b"incorrect\n", b"\xff"):
                with self.subTest(corrupt=corrupt[-32:]):
                    self.command.write_bytes(corrupt)
                    self.check_reference(False)
                    self.generate()
                    self.assertEqual(self.command.read_bytes(), correct)
                    self.check_reference(True)

        def test_configuration_drift_cannot_be_silently_regenerated(self):
            self.generate()
            original = self.configuration.read_bytes()
            for bad in (original + b"\nStale configuration sentinel.\n", b"incorrect\n", b"\xff", None):
                with self.subTest(missing=bad is None):
                    if bad is None:
                        self.configuration.unlink()
                    else:
                        self.configuration.write_bytes(bad)
                    self.check_reference(False)
                    before = self.snapshot()
                    self.generate(False)
                    self.assertEqual(self.snapshot(), before, "invalid config must fail before writes")
                    self.configuration.write_bytes(original)
            self.generate()
            self.check_reference(True)

    return unittest.defaultTestLoader.loadTestsFromTestCase(ReferenceLifecycle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference-test-binary", type=Path)
    parser.add_argument("--generator-binary", type=Path)
    args = parser.parse_args()
    if bool(args.reference_test_binary) != bool(args.generator_binary):
        parser.error("supply both compiled binaries, or neither for offline policy tests")
    if args.reference_test_binary:
        suite = runtime_suite(str(args.reference_test_binary.resolve()),
                              str(args.generator_binary.resolve()))
    else:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(GeneratedCommandReferencePolicy)
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

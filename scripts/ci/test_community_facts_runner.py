"""Offline snapshot/prerequisite regressions for the protected facts runner.

Run the real runner and canonical command-reference script in a tiny Git fixture.
Only the Cargo boundary and its compiled generator artifact are doubles: no real
Cargo command or cache admission is allowed. Existing scripts/ci discovery runs
this suite without workflow changes.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
RUNNER = "crates/discord/tests/run_community_facts_requeue.py"
GENERATOR = "scripts/generate_command_reference.py"
SOURCE = "crates/discord/src/leveling_runtime.rs"
PROTECTED = "crates/discord/tests/support/community_facts_requeue.rs"
REFERENCE = "docs/generated/commands.md"
GENERATED = "# Snapshot command reference\n"

BOUNDARY = r'''
import json
import os
from pathlib import Path
import sys

root = Path.cwd()
# Mirror the compiled renderer: the environment overrides its manifest-root fallback.
reference_root = Path(os.environ.get("REFERENCE_DOCS_ROOT",
                                    str(Path(__file__).resolve().parents[1])))
args = sys.argv[1:]
name = Path(__file__).name
reference = root / "docs/generated/commands.md"
selected_reference = reference_root / "docs/generated/commands.md"
if name == "cargo":
    stage = "unbounded-cargo"
elif name == "cargo_cache.py":
    stage = "build" if args[:3] == ["run", "--", "build"] else "compile"
else:
    stage = "generate"
with Path(os.environ["FACTS_RUNNER_EVENTS"]).open("a") as log:
    log.write(json.dumps({"stage": stage, "cwd": str(root), "args": args,
                          "reference_root": str(reference_root),
                          "reference": reference.read_text() if reference.exists() else None}) + "\n")
if stage == "unbounded-cargo":
    raise SystemExit("real Cargo is forbidden in this offline fixture")
status = int(os.environ.get("FACTS_RUNNER_" + stage.upper() + "_STATUS", "0"))
if status:
    raise SystemExit(status)
if stage == "build":
    if args != ["run", "--", "build", "-p", "two-bot-core", "--example",
                "generate_command_reference", "--locked", "--message-format=json"]:
        raise SystemExit("unexpected generator build")
    print(json.dumps({"reason": "compiler-artifact",
                      "target": {"name": "generate_command_reference", "kind": ["example"]},
                      "executable": str(root / "fixture/generate_command_reference")}))
elif stage == "generate":
    if (reference_root / "docs/configuration.md").read_text() != "# Fixture configuration reference\n":
        raise SystemExit("generation requires a matching configuration reference")
    selected_reference.parent.mkdir(parents=True, exist_ok=True)
    selected_reference.write_text("# Snapshot command reference\n")
    print(json.dumps({"schema": "command_reference.v1", "status": "ok"}))
elif "--workspace" in args:
    if not reference.exists() or reference.read_text() != "# Snapshot command reference\n":
        print("workspace compilation requires its own generated reference", file=sys.stderr)
        raise SystemExit(86)
    if (not selected_reference.exists()
            or selected_reference.read_text() != "# Snapshot command reference\n"):
        print("workspace verification requires its own generated reference", file=sys.stderr)
        raise SystemExit(87)
'''


class CommunityFactsRunnerPrerequisites(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get(
            "PAPERCLIP_SCRATCH_DIR")
        temporary = tempfile.TemporaryDirectory(prefix="facts-runner-fixture-", dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.fixture = Path(temporary.name).resolve()
        self.checkout = self.fixture / "checkout"
        self.checkout.mkdir()
        self.scratch = self.fixture / "snapshots"
        self.scratch.mkdir()
        self.events = self.fixture / "events.jsonl"
        for path in (RUNNER, GENERATOR):
            destination = self.checkout / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / path, destination)
        self.source = b"use fixture::{message_fact, record_fact, rules_accepted_fact,};\n"
        for path, contents in (
            (SOURCE, self.source), (PROTECTED, b"// protected corpus sentinel\n"),
            ("docs/configuration.md", b"# Fixture configuration reference\n"),
            ("docs/generated/.gitignore", b"/commands.md\n"),
        ):
            destination = self.checkout / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(contents)
        for path in ("scripts/cargo_cache.py", "fixture/generate_command_reference", "bin/cargo"):
            destination = self.checkout / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(f"#!{sys.executable}\n" + BOUNDARY)
            destination.chmod(0o755)
        for args in (("init", "--quiet"), ("add", ".")):
            subprocess.run(["git", *args], cwd=self.checkout, check=True,
                           capture_output=True, timeout=10)
        self.env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1",
                        PAPERCLIP_RUN_SCRATCH_DIR=str(self.scratch),
                        FACTS_RUNNER_EVENTS=str(self.events))
        # Keep inherited-root cases inside this fixture, never the caller's checkout.
        self.env.pop("REFERENCE_DOCS_ROOT", None)
        # Require explicit --controller; accidental direct Cargo is a refusing stub.
        self.env.pop("PAPERCLIP_RUN_ID", None)
        self.env["PATH"] = str(self.checkout / "bin") + os.pathsep + os.environ["PATH"]

    def run_runner(self, *args, **environment):
        result = subprocess.run(
            [sys.executable, str(self.checkout / RUNNER), *args],
            cwd=self.fixture, env=dict(self.env, **environment), capture_output=True,
            text=True, check=False, timeout=30)
        events = [json.loads(line) for line in self.events.read_text().splitlines()]
        self.events.unlink()
        self.assertEqual((self.checkout / SOURCE).read_bytes(), self.source)
        self.assertEqual(list(self.scratch.iterdir()), [], "temporary snapshot must be cleaned up")
        return result, events

    def assert_snapshot_events(self, events, stages):
        self.assertEqual([event["stage"] for event in events], stages)
        workspace = Path(events[0]["cwd"])
        self.assertEqual(workspace.parent, self.scratch)
        self.assertNotEqual(workspace, self.checkout)
        self.assertTrue(all(event["cwd"] == str(workspace) for event in events))
        self.assertTrue(all(event["reference_root"] == str(workspace) for event in events),
                        "generation and verification must select the snapshot reference root")

    def assert_workspace_success(self, result, events):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_snapshot_events(events, ["build", "generate", "compile"])
        self.assertIsNone(events[0]["reference"], "ignored source output must not be copied")
        self.assertIsNone(events[1]["reference"])
        self.assertEqual(events[2]["reference"], GENERATED)
        self.assertEqual(events[2]["args"], [
            "run", "--", "test", "--workspace", "--features",
            "two-bot-discord/db", "--locked"])

    def test_workspace_generates_in_snapshot_before_compilation(self):
        for source_reference in (None, "# Stale ignored source-checkout output\n"):
            with self.subTest(source_reference=source_reference):
                reference = self.checkout / REFERENCE
                if source_reference is not None:
                    reference.write_text(source_reference)
                result, events = self.run_runner("--workspace")
                self.assert_workspace_success(result, events)
                self.assertEqual(reference.read_text() if reference.exists() else None,
                                 source_reference, "generation must not modify the source checkout")

    def test_workspace_ignores_inherited_source_reference_root(self):
        for source_reference in (None, "# Stale ignored source-checkout output\n"):
            with self.subTest(source_reference=source_reference):
                reference = self.checkout / REFERENCE
                if source_reference is not None:
                    reference.write_text(source_reference)
                result, events = self.run_runner("--workspace", REFERENCE_DOCS_ROOT=str(self.checkout))
                self.assertEqual(reference.read_text() if reference.exists() else None,
                                 source_reference, "generation must not modify the inherited source root")
                self.assert_workspace_success(result, events)

    def test_workspace_ignores_inherited_external_reference_root(self):
        external = self.fixture / "external"
        shutil.copytree(self.checkout / "docs", external / "docs")
        for source_reference in (None, "# Stale ignored source-checkout output\n"):
            for external_reference in (None, "# Preserve external-checkout output\n"):
                with self.subTest(source_reference=source_reference, external_reference=external_reference):
                    source = self.checkout / REFERENCE
                    reference = external / REFERENCE
                    if source_reference is not None:
                        source.write_text(source_reference)
                    if external_reference is None:
                        reference.unlink(missing_ok=True)
                    else:
                        reference.write_text(external_reference)
                    result, events = self.run_runner("--workspace", REFERENCE_DOCS_ROOT=str(external))
                    self.assertEqual(reference.read_text() if reference.exists() else None,
                                     external_reference, "generation must not modify the inherited external root")
                    self.assertEqual(source.read_text() if source.exists() else None,
                                     source_reference, "generation must not modify the source checkout")
                    self.assert_workspace_success(result, events)

    def test_generator_build_failure_propagates_without_workspace_compilation(self):
        for status in (1, 42, 75):
            with self.subTest(status=status):
                result, events = self.run_runner(
                    "--workspace", FACTS_RUNNER_BUILD_STATUS=str(status),
                    REFERENCE_DOCS_ROOT=str(self.checkout))
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                self.assert_snapshot_events(events, ["build"])
                self.assertFalse((self.checkout / REFERENCE).exists())

    def test_generator_execution_failure_propagates_without_workspace_compilation(self):
        for status in (1, 75):
            with self.subTest(status=status):
                result, events = self.run_runner("--workspace", FACTS_RUNNER_GENERATE_STATUS=str(status))
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                self.assert_snapshot_events(events, ["build", "generate"])
                self.assertFalse((self.checkout / REFERENCE).exists())

    def test_workspace_compilation_exit_status_is_preserved(self):
        for status in (1, 42, 75):
            with self.subTest(status=status):
                result, events = self.run_runner(
                    "--workspace", FACTS_RUNNER_COMPILE_STATUS=str(status),
                    REFERENCE_DOCS_ROOT=str(self.checkout))
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                self.assert_snapshot_events(events, ["build", "generate", "compile"])
                self.assertEqual(events[-1]["reference"], GENERATED)
                self.assertFalse((self.checkout / REFERENCE).exists())

    def test_discord_only_modes_do_not_generate_bot_reference(self):
        for args, command in (((), "test"), (("--controls",), "test"), (("--clippy",), "clippy")):
            with self.subTest(args=args):
                result, events = self.run_runner(*args, FACTS_RUNNER_BUILD_STATUS="75")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_snapshot_events(events, ["compile"])
                self.assertEqual(events[0]["args"][:4], ["run", "--", command, "-p"])
                self.assertEqual(events[0]["args"][4], "two-bot-discord")
                self.assertIsNone(events[0]["reference"])
                self.assertFalse((self.checkout / REFERENCE).exists())


if __name__ == "__main__":
    unittest.main()

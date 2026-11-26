"""Offline regression tests for producer/consumer coverage workspace boundaries."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import yaml


ROOT = Path(__file__).resolve().parents[2]
CONTAINER_ROOT = "/__w/two-bot-next/two-bot-next"
SOURCE = "crates/core/src/action_outcomes.rs"
INVALID_SOURCE_ROOT_CASES = (
    ("/", f"/{SOURCE}"),
    ("relative/path", SOURCE),
    ("/producer/../checkout", SOURCE),
)


def load_script(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts/ci" / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MERGE = load_script("boundary_merge", "merge-coverage-shards.py")
GATE = load_script("boundary_gate", "check-patch-coverage.py")


class CoverageShardBoundaryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        )
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)

    def report(self, name, source, counts):
        path = self.directory / name
        path.write_text(
            f"TN:\nSF:{source}\n"
            + "".join(f"DA:{line},{count}\n" for line, count in counts.items())
            + "end_of_record\n"
        )
        return path

    def merge_cli(self, reports, roots=()):
        output = self.directory / "merged.info"
        args = [sys.executable, str(ROOT / "scripts/ci/merge-coverage-shards.py")]
        for report in reports:
            args.extend(["--report", str(report)])
        for root in roots:
            args.extend(["--source-root", str(root)])
        args.extend(["--expected-shards", str(len(reports)), "--out", str(output)])
        result = subprocess.run(
            args, cwd=ROOT, capture_output=True, text=True, check=False, timeout=15
        )
        return result, output

    def read_hits(self, report):
        hits, error = GATE.read_lcov(report)
        self.assertIsNotNone(hits, error)
        self.assertEqual(error, "")
        return hits

    def real_rust_diff(self):
        # Commit real repository source in a disposable repo, change three
        # executable statements, and use the gate's actual git-diff parser.
        # Neither the working Rust source nor its history is modified.
        repo = self.directory / "diff-repo"
        source = repo / SOURCE
        source.parent.mkdir(parents=True)
        before = (ROOT / SOURCE).read_text()
        source.write_text(before)

        def git(*args):
            return subprocess.run(
                ["git", "-C", str(repo), *args], capture_output=True, text=True,
                check=True, timeout=15,
            ).stdout.strip()

        git("init", "--quiet")
        git("add", SOURCE)
        git("-c", "user.name=Coverage Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture baseline")
        base = git("rev-parse", "HEAD")
        after = before
        replacements = (
            ("matches!(self, Self::Warned { .. })", "matches!(self, Self::Warned { user_id: _ })"),
            ("mask | SEND_MESSAGES_BIT", "mask | (SEND_MESSAGES_BIT)"),
            ("mask & !SEND_MESSAGES_BIT", "mask & !(SEND_MESSAGES_BIT)"),
        )
        for old, new in replacements:
            self.assertEqual(after.count(old), 1, old)
            after = after.replace(old, new)
        source.write_text(after)
        git("add", SOURCE)
        git("-c", "user.name=Coverage Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "change executable statements")
        with patch.object(GATE, "ROOT", repo):
            added, error = GATE.added_lines(base, "HEAD")
        self.assertEqual(error, "")
        expected = {
            (SOURCE, number)
            for number, line in enumerate(after.splitlines(), 1)
            if any(new in line for _, new in replacements)
        }
        self.assertEqual(added, expected)
        self.assertEqual(len(added), 3)
        return repo, added, sorted(number for _, number in added)

    def test_container_and_relative_union_matches_host_single_job_on_real_rust_diff(self):
        repo, added, (first, second, third) = self.real_rust_diff()
        for final_count in (0, 1):
            with self.subTest(final_count=final_count):
                producer = self.report("container.info", f"{CONTAINER_ROOT}/{SOURCE}",
                                       {first: 2, second: 0, third: 0})
                relative = self.report("relative.info", SOURCE,
                                       {first: 3, second: 4, third: final_count})
                expected = {(SOURCE, first): 5, (SOURCE, second): 4,
                            (SOURCE, third): final_count}
                host = self.report("single-job.info", repo / SOURCE,
                                   {first: 5, second: 4, third: final_count})
                with patch.object(GATE, "ROOT", repo):
                    single_verdict = GATE.evaluate(self.read_hits(host), added)
                expected_verdict = (3, 3, []) if final_count else (2, 3, [(SOURCE, third)])
                self.assertEqual(single_verdict, expected_verdict)
                result, output = self.merge_cli([producer, relative], [CONTAINER_ROOT])
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                merged = self.read_hits(output)
                self.assertEqual(merged, expected)
                self.assertEqual(
                    MERGE.merge_lcov([producer, relative], source_roots=(CONTAINER_ROOT,)),
                    expected,
                )
                with patch.object(GATE, "ROOT", repo):
                    sharded_verdict = GATE.evaluate(merged, added)
                self.assertEqual(sharded_verdict, single_verdict)
                # Parity includes both sides of the unchanged 80% HARD bar.
                self.assertEqual(sharded_verdict[0] / sharded_verdict[1] >= 0.8,
                                 bool(final_count))

    def test_default_root_unions_host_absolute_and_relative_paths(self):
        absolute = self.report("host.info", ROOT / SOURCE, {129: 2, 130: 0})
        relative = self.report("relative.info", SOURCE, {129: 3, 130: 1})
        result, output = self.merge_cli([absolute, relative])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        expected = {(SOURCE, 129): 5, (SOURCE, 130): 1}
        self.assertEqual(self.read_hits(output), expected)
        self.assertEqual(MERGE.merge_lcov([absolute, relative]), expected)

    def test_relative_source_compatibility_without_configured_roots(self):
        first = self.report("first.info", SOURCE, {129: 2, 130: 0})
        second = self.report("second.info", SOURCE, {129: 3, 130: 1})
        expected = {(SOURCE, 129): 5, (SOURCE, 130): 1}
        self.assertEqual(MERGE.merge_lcov([first, second]), expected)
        result, output = self.merge_cli([first, second])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.read_hits(output), expected)

    def test_repeated_source_roots_union_multiple_producer_workspaces(self):
        roots = (CONTAINER_ROOT, "/opt/coverage-producer/two-bot-next")
        reports = [self.report(f"producer-{index}.info", f"{root}/{SOURCE}", {129: index + 1})
                   for index, root in enumerate(roots)]
        result, output = self.merge_cli(reports, roots)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        expected = {(SOURCE, 129): 3}
        self.assertEqual(self.read_hits(output), expected)
        self.assertEqual(MERGE.merge_lcov(reports, source_roots=roots), expected)

    def test_cli_rejects_invalid_source_roots_without_output(self):
        # A slash root must not launder a foreign absolute SF into this checkout.
        for root, source in INVALID_SOURCE_ROOT_CASES:
            with self.subTest(root=root):
                report = self.report("invalid-root.info", source, {129: 1})
                result, output = self.merge_cli([report], [root])
                diagnostic = result.stdout + result.stderr
                self.assertEqual(result.returncode, 1, diagnostic)
                self.assertIn(f"invalid LCOV source root: {root!r}", diagnostic)
                self.assertIn("::error::", diagnostic)
                self.assertIn("failing closed", diagnostic)
                self.assertFalse(output.exists(), "invalid roots must not publish coverage")

    def test_api_rejects_invalid_source_roots(self):
        for root, source in INVALID_SOURCE_ROOT_CASES:
            with self.subTest(root=root):
                report = self.report("invalid-root.info", source, {129: 1})
                with self.assertRaises(ValueError) as raised:
                    MERGE.merge_lcov([report], source_roots=(root,))
                self.assertEqual(str(raised.exception),
                                 f"invalid LCOV source root: {root!r}")

    def test_cli_rejects_outside_root_and_traversal_without_partial_output(self):
        valid = self.report("valid.info", SOURCE, {129: 1})
        for invalid in (
            f"/outside-checkout/{SOURCE}",
            f"{CONTAINER_ROOT}-other/{SOURCE}",
            f"{CONTAINER_ROOT}/../outside/{SOURCE}",
            f"../{SOURCE}",
            "crates/core/src/../../../../outside.rs",
        ):
            with self.subTest(source=invalid):
                bad = self.report("invalid.info", invalid, {129: 7})
                result, output = self.merge_cli([valid, bad], [CONTAINER_ROOT])
                # A parser error for an unimplemented flag is not path rejection.
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("::error::", result.stdout + result.stderr)
                self.assertIn("failing closed", result.stdout + result.stderr)
                self.assertNotIn("unrecognized arguments", result.stderr)
                self.assertFalse(output.exists(), "partial coverage must not reach the gate")

    def test_api_rejects_outside_configured_roots(self):
        for invalid in (f"/outside-checkout/{SOURCE}", f"{CONTAINER_ROOT}-other/{SOURCE}"):
            with self.subTest(source=invalid):
                bad = self.report("invalid.info", invalid, {129: 1})
                with self.assertRaises(ValueError):
                    MERGE.merge_lcov([bad], source_roots=(CONTAINER_ROOT,))

    def test_default_cli_and_api_reject_untrusted_paths(self):
        # These negatives use the pre-existing CLI/API too: no new flag can
        # mask accidental acceptance of traversal or an unrelated absolute SF.
        for invalid in (f"/outside-checkout/{SOURCE}", f"../{SOURCE}",
                        "crates/core/src/../../../../outside.rs"):
            with self.subTest(source=invalid, interface="api"):
                bad = self.report("invalid.info", invalid, {129: 1})
                with self.assertRaises(ValueError):
                    MERGE.merge_lcov([bad])
            with self.subTest(source=invalid, interface="cli"):
                result, output = self.merge_cli([bad])
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("failing closed", result.stdout + result.stderr)
                self.assertFalse(output.exists())


class CoverageRetryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = yaml.load((ROOT / ".github/workflows/check.yml").read_text(),
                             Loader=yaml.BaseLoader)
        cls.consumer = {step["name"]: step for step in workflow["jobs"]["patch-coverage"]["steps"]
                        if "name" in step}
        cls.producer = {step["name"]: step for step in workflow["jobs"]["coverage-shard"]["steps"]
                        if "name" in step}

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        (self.directory / "scripts").symlink_to(ROOT / "scripts", target_is_directory=True)
        self.git("init", "--quiet")
        self.source = self.commit("source under test")
        self.selector = '{"rust":"true","worker":"false"}'
        self.plan = json.dumps({"include": [
            {"shard_index": 0, "legs": "sweep-nonignored"},
            {"shard_index": 1, "legs": "sweep-ignored"},
        ]})

    def git(self, *args):
        return subprocess.run(
            ["git", "-C", str(self.directory), *args], capture_output=True,
            text=True, check=True, timeout=15,
        ).stdout.strip()

    def commit(self, message):
        self.git("-c", "user.name=Coverage Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def step(self, step, attempt, **overrides):
        env = dict(os.environ, GITHUB_WORKSPACE=str(self.directory), GITHUB_SHA=self.source,
                   GITHUB_RUN_ID="12345", GITHUB_RUN_ATTEMPT=str(attempt),
                   COVERAGE_SELECTOR=self.selector, COVERAGE_MATRIX=self.plan, EXPECTED_SHARDS="2")
        env.update(overrides)
        return subprocess.run(["bash", "-e", "-o", "pipefail", "-c", step["run"]],
                              cwd=self.directory, env=env, capture_output=True, text=True,
                              check=False, timeout=15)

    def upload(self, index, attempt, hits, **overrides):
        report = self.directory / f"lcov-shard-{index}.info"
        report.write_text(f"TN:\nSF:{CONTAINER_ROOT}/{SOURCE}\nDA:129,{hits}\nend_of_record\n")
        if stamp := self.producer.get("Bind shard coverage report"):
            plan = json.loads(overrides.get("COVERAGE_MATRIX", self.plan))
            legs = next(entry["legs"] for entry in plan["include"] if entry["shard_index"] == index)
            result = self.step(stamp, attempt, SHARD_INDEX=str(index),
                               SHARD_LEGS=legs, **overrides)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        download = self.consumer["Download shard coverage reports"]["with"]
        destination = self.directory / download["path"]
        if download.get("merge-multiple") != "true":
            destination /= f"coverage-shard-{index}"
        destination.mkdir(parents=True, exist_ok=True)
        upload = self.producer["Upload shard coverage report"]["with"]
        for filename in upload["path"].splitlines():
            path = self.directory / filename.replace("${{ matrix.shard_index }}", str(index))
            path.replace(destination / path.name)
        return destination / report.name

    def test_workflow_rejects_foreign_retry_artifacts(self):
        foreign = {
            "selector": {"COVERAGE_SELECTOR": '{"rust":"true","worker":"true"}'},
            "assigned legs": {"COVERAGE_MATRIX": self.plan.replace("sweep-nonignored", "discord-internal_events")},
            "other shard legs": {"COVERAGE_MATRIX": self.plan.replace('"sweep-ignored"', '"sweep-ignored discord-internal_events"')},
        }
        for name, overrides in foreign.items():
            with self.subTest(changed=name):
                # Shard 0 is retained and is the only evidence of a hit. Shard 1
                # is replaced. Both reports match the current count and index set.
                self.upload(0, 1, hits=1, **overrides)
                self.upload(1, 2, hits=0)
                result = self.step(self.consumer["Merge coverage shards"], 2)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("coverage shard 0 identity mismatch", result.stdout)
                self.assertFalse((self.directory / "lcov.info").exists())

    def test_workflow_accepts_exact_retained_retry_artifact(self):
        self.upload(0, 1, hits=1)
        # The retry's replacement overwrites the old artifact at the same index.
        self.upload(1, 1, hits=3)
        self.upload(1, 2, hits=0, COVERAGE_SELECTOR='{"worker":"false","rust":"true"}')
        result = self.step(self.consumer["Merge coverage shards"], 2)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        hits, error = GATE.read_lcov(self.directory / "lcov.info")
        self.assertEqual(error, "")
        self.assertEqual(hits, {(SOURCE, 129): 1})
        self.assertEqual(GATE.evaluate(hits, {(SOURCE, 129)}), (1, 1, []))

    def test_workflow_rejects_retained_report_from_another_checkout(self):
        self.upload(0, 1, hits=1)
        self.source = self.commit("different source")
        self.upload(1, 2, hits=0)
        result = self.step(self.consumer["Merge coverage shards"], 2)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("coverage shard 0 identity mismatch", result.stdout)
        self.assertFalse((self.directory / "lcov.info").exists())

    def test_workflow_rejects_missing_or_mismatched_artifact_evidence(self):
        merge = self.consumer["Merge coverage shards"]
        first = self.upload(0, 1, hits=1)
        result = self.step(merge, 2)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("do not match the 2 planned indices", result.stdout)
        self.assertFalse((self.directory / "lcov.info").exists())
        second = self.upload(1, 2, hits=0)
        first.write_bytes(second.read_bytes())
        result = self.step(merge, 2)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("coverage shard 0 identity mismatch", result.stdout)
        self.assertFalse((self.directory / "lcov.info").exists())
        first = self.upload(0, 1, hits=1)
        first.with_suffix(".identity").unlink()
        result = self.step(merge, 2)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("do not match the 2 planned indices", result.stdout)
        self.assertFalse((self.directory / "lcov.info").exists())
        first = self.upload(0, 1, hits=1)
        first.with_suffix(".identity").rename(first.with_name("lcov-shard-2.identity"))
        first.rename(first.with_name("lcov-shard-2.info"))
        result = self.step(merge, 2)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("do not match the 2 planned indices", result.stdout)
        self.assertFalse((self.directory / "lcov.info").exists())

    def test_workflow_accepts_single_nonempty_shard(self):
        self.plan = json.dumps({"include": [{"shard_index": 0, "legs": "sweep-nonignored"}]})
        self.upload(0, 1, hits=1, EXPECTED_SHARDS="1")
        result = self.step(self.consumer["Merge coverage shards"], 2, EXPECTED_SHARDS="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_publisher_rejects_checkout_and_assignment_mismatches(self):
        stamp = self.producer["Bind shard coverage report"]
        result = self.step(stamp, 2, SHARD_INDEX="0", SHARD_LEGS="sweep-ignored")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("executed coverage shard 0 does not match the matrix", result.stdout)
        result = self.step(stamp, 2, SHARD_INDEX="0", SHARD_LEGS="sweep-nonignored",
                           GITHUB_SHA="a" * 40)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("coverage checkout does not match the workflow SHA", result.stdout)
        self.assertFalse((self.directory / "lcov-shard-0.identity").exists())


if __name__ == "__main__":
    unittest.main()

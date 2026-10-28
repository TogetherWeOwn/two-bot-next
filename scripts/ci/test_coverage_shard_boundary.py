"""Offline regression tests for producer/consumer coverage workspace boundaries."""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


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


if __name__ == "__main__":
    unittest.main()

"""Offline tests for the unwrap request-path HARD gate.

The gate must fail on new `.unwrap()` in scope, stay silent when the diff
adds none, warn (never fail) on new shared-advisory findings only, and fail
closed when it is blind. Fixtures use a throwaway git repo and a fake
ast-grep binary; the real rule file shape is pinned so the gate cannot
drift off the committed rule.
"""

import io
import json
import stat
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch
from contextlib import redirect_stdout
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]

import importlib.util

SPEC = importlib.util.spec_from_file_location(
    "unwrap_gate", Path(__file__).with_name("check-unwrap-request-paths.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


SCAN_FIXTURE = [
    {"ruleId": "no-unwrap-request-paths",
     "file": "crates/bot/src/gateway_commands.rs",
     "range": {"start": {"line": 10}}},
    {"ruleId": "no-unwrap-request-paths",
     "file": "crates/bot/src/gateway_commands.rs",
     "range": {"start": {"line": 99}}},
    {"ruleId": "some-other-rule",
     "file": "crates/bot/src/gateway_commands.rs",
     "range": {"start": {"line": 11}}},
]


def make_repo():
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "config", "user.email", "t@t"], cwd=root, check=True)
    subprocess.run(["git", "config", "user.name", "t"], cwd=root, check=True)
    return tmp, root


def commit(root, files):
    for path, content in files.items():
        full = root / path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "x"], cwd=root, check=True)
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                          capture_output=True, text=True, check=True).stdout.strip()


def fake_ast_grep(payload):
    import shlex
    tmp = tempfile.TemporaryDirectory()
    fixture = Path(tmp.name) / "scan.json"
    fixture.write_text(payload)
    binary = Path(tmp.name) / "ast-grep"
    binary.write_text("#!/bin/sh\ncat %s\n" % shlex.quote(str(fixture)))
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return tmp, str(binary)


class ScanTests(unittest.TestCase):
    def test_scan_downgrades_all_rules_to_warnings(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(SCAN_FIXTURE), stderr="")) as run:
            hits, error = gate.run_scan("ast-grep", all_rules=True)
        self.assertEqual(error, "")
        self.assertEqual(len(hits), 3)
        self.assertIn("--warning", run.call_args.args[0])

    def test_scan_converts_zero_based_lines_to_one_based(self):
        fixture = [{"ruleId": gate.RULE_ID, "file": "a.rs",
                    "range": {"start": {"line": 0}}}]
        tmp, binary = fake_ast_grep(json.dumps(fixture))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(hits, [("a.rs", 1)])

    def test_scan_keeps_only_rule_hits_with_line_numbers(self):
        tmp, binary = fake_ast_grep(json.dumps(SCAN_FIXTURE))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(hits, [("crates/bot/src/gateway_commands.rs", 11),
                                ("crates/bot/src/gateway_commands.rs", 100)])

    def test_scan_keeps_duplicate_line_findings_for_the_backlog_count(self):
        fixture = [dict(SCAN_FIXTURE[0]), dict(SCAN_FIXTURE[0])]
        tmp, binary = fake_ast_grep(json.dumps(fixture))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(len(hits), 2)

    def test_all_rules_scan_includes_advisory_rule_ids(self):
        tmp, binary = fake_ast_grep(json.dumps(SCAN_FIXTURE))
        try:
            hits, error = gate.run_scan(binary, all_rules=True)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertIn(("some-other-rule", "crates/bot/src/gateway_commands.rs", 12), hits)

    def test_scan_reports_garbage_instead_of_crashing(self):
        tmp, binary = fake_ast_grep("not json")
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertIsNone(hits)
        self.assertIn("not JSON", error)

    def test_missing_binary_is_blindness_not_a_crash(self):
        hits, error = gate.run_scan("/nonexistent/ast-grep-binary")
        self.assertIsNone(hits)
        self.assertTrue(error)

    def test_nonzero_exit_with_empty_output_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=1, stdout="", stderr="scan failed")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("exited 1", error)

    def test_empty_success_output_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout="", stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("empty", error)

    def test_malformed_json_shape_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps({"findings": []}), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid shape", error)

    def test_malformed_finding_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps([None]), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid finding", error)

    def test_finding_without_line_is_blindness(self):
        finding = {"ruleId": gate.RULE_ID, "file": "a.rs", "range": {"start": {}}}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps([finding]), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid finding", error)

    def test_boolean_line_is_blindness(self):
        finding = {"ruleId": gate.RULE_ID, "file": "a.rs",
                   "range": {"start": {"line": True}}}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps([finding]), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid finding", error)


class DiffTests(unittest.TestCase):
    def test_added_lines_tracks_new_file_lines(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {"a.rs": "one\n"})
            head = commit(root, {"a.rs": "one\nlet x = y.unwrap();\nthree\n"})
            real_root, gate.ROOT = gate.ROOT, root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {("a.rs", 2), ("a.rs", 3)})

    def test_pure_rename_does_not_add_every_line(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {"interactions.rs": "one\ntwo\nthree\n"})
            (root / "interactions.rs").rename(root / "rooms.rs")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "rename"], cwd=root, check=True)
            head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                                  capture_output=True, text=True, check=True).stdout.strip()
            real_root, gate.ROOT = gate.ROOT, root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, set())

    def test_bad_refs_are_reported(self):
        real_root, gate.ROOT = gate.ROOT, Path(tempfile.mkdtemp())
        try:
            added, error = gate.added_lines("nope", "nada")
        finally:
            gate.ROOT = real_root
        self.assertIsNone(added)
        self.assertTrue(error)


class GateTests(unittest.TestCase):
    def run_gate(self, scan, base, head):
        tmp, binary = fake_ast_grep(json.dumps(scan))
        real_root, gate.ROOT = gate.ROOT, Path(".")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--base-ref", base, "--head-ref", head,
                                  "--ast-grep", binary])
        finally:
            gate.ROOT = real_root
            tmp.cleanup()
        return code, out.getvalue()

    def test_inline_test_hit_uses_in_scope_diagnostic(self):
        real_scan, real_added = gate.run_scan, gate.added_lines
        gate.run_scan = lambda _binary, all_rules=False: (
            [(gate.RULE_ID, "crates/bot/src/gateway_commands.rs", 12)], "")
        gate.added_lines = lambda _base, _head: (
            {("crates/bot/src/gateway_commands.rs", 12)}, "")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--base-ref", "base", "--head-ref", "head"])
        finally:
            gate.run_scan, gate.added_lines = real_scan, real_added
        self.assertEqual(code, 1)
        self.assertIn("inline test modules are included", out.getvalue())
        self.assertNotIn("non-test request path", out.getvalue())

    def test_fresh_unwrap_fails(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {"crates/bot/src/gateway_commands.rs": "a\n"})
            head = commit(root, {"crates/bot/src/gateway_commands.rs": "a\nb.unwrap();\n"})
            real_root, gate.ROOT = gate.ROOT, root
            scan = [{"ruleId": "no-unwrap-request-paths",
                     "file": "crates/bot/src/gateway_commands.rs",
                     "range": {"start": {"line": 1}}}]
            fake_tmp, binary = fake_ast_grep(json.dumps(scan))
            out = io.StringIO()
            try:
                with redirect_stdout(out):
                    code = gate.main(["--base-ref", base, "--head-ref", head,
                                      "--ast-grep", binary])
            finally:
                fake_tmp.cleanup()
                gate.ROOT = real_root
        finally:
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())
        self.assertIn("gateway_commands.rs:2", out.getvalue())

    def test_preexisting_hit_is_silent(self):
        code, output = self.run_gate(SCAN_FIXTURE, "HEAD", "HEAD")
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)
        self.assertNotIn("::warning::", output)

    def test_new_shared_advisory_warns_but_passes(self):
        real_scan, real_added = gate.run_scan, gate.added_lines
        gate.run_scan = lambda _binary, all_rules=False: (
            [("rust-erased-map-err", "crates/core/src/example.rs", 7)], "")
        gate.added_lines = lambda _base, _head: ({("crates/core/src/example.rs", 7)}, "")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--base-ref", "base", "--head-ref", "head"])
        finally:
            gate.run_scan, gate.added_lines = real_scan, real_added
        self.assertEqual(code, 0)
        self.assertIn("rust-erased-map-err", out.getvalue())
        self.assertIn("::warning::", out.getvalue())

    def test_blind_gate_fails_closed(self):
        out = io.StringIO()
        with redirect_stdout(out):
            code = gate.main(["--base-ref", "HEAD", "--head-ref", "HEAD",
                              "--ast-grep", "/nonexistent/ast-grep-binary"])
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())
        self.assertIn("blind", out.getvalue())

    def test_unreadable_diff_fails_closed(self):
        real_root, gate.ROOT = gate.ROOT, Path(tempfile.mkdtemp())
        tmp, binary = fake_ast_grep(json.dumps(SCAN_FIXTURE))
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--base-ref", "nope", "--head-ref", "nada",
                                  "--ast-grep", binary])
        finally:
            gate.ROOT = real_root
            tmp.cleanup()
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())


class RuleShapeTests(unittest.TestCase):
    def test_rule_targets_request_paths_and_ignores_tests(self):
        rule = yaml.safe_load(
            (ROOT / "ast-grep-rules/no-unwrap-request-paths.yml").read_text())
        self.assertEqual(rule["id"], "no-unwrap-request-paths")
        self.assertEqual(rule["language"], "rust")
        self.assertEqual(rule["rule"], {"pattern": "$X.unwrap()"})
        files = " ".join(rule["files"])
        for keyword in ("route", "handler", "interaction", "command", "webhook"):
            self.assertIn(keyword, files)
        ignores = " ".join(rule["ignores"])
        for marker in ("_tests", "/tests/"):
            self.assertIn(marker, ignores)
        self.assertNotIn("cfg(test)", ignores)
        self.assertIn("inline test modules", rule["message"])

    def test_backlog_documents_inline_test_module_enforcement(self):
        backlog = (ROOT / "docs/unwrap-backlog.md").read_text()
        self.assertIn("no-unwrap-request-paths", backlog)
        self.assertIn("inline `#[cfg(test)]` modules are included", backlog)
        self.assertIn("those hits are not filtered from enforcement", backlog)

    def test_shared_rust_advisories_have_local_rules_and_fixtures(self):
        rule_ids = {
            "rust-byte-split-offset",
            "rust-dropped-wait-for-future",
            "rust-erased-map-err",
            "rust-test-global-env",
            "rust-test-global-metrics",
            "rust-wildcard-access-denied",
        }
        for rule_id in rule_ids:
            rule = ROOT / f"ast-grep-rules/{rule_id}.yml"
            fixture = ROOT / f"ast-grep-rule-tests/{rule_id}-test.yml"
            self.assertTrue(rule.is_file(), rule)
            self.assertTrue(fixture.is_file(), fixture)
            self.assertEqual(yaml.safe_load(rule.read_text())["id"], rule_id)


if __name__ == "__main__":
    unittest.main()

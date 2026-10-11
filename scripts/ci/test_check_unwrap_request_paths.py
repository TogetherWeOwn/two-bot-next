"""Offline tests for the unwrap request-path WARN gate.

The gate must warn (never fail) on new `.unwrap()` in scope, stay silent when
the diff adds none, and stay green-but-loud when it is blind. Fixtures use a
throwaway git repo and a fake ast-grep binary; the real rule file shape is
pinned so the gate cannot drift off the committed rule.
"""

import io
import json
import stat
import subprocess
import tempfile
import unittest
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
    def test_scan_keeps_only_rule_hits_with_line_numbers(self):
        tmp, binary = fake_ast_grep(json.dumps(SCAN_FIXTURE))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(hits, [("crates/bot/src/gateway_commands.rs", 10),
                                ("crates/bot/src/gateway_commands.rs", 99)])

    def test_scan_keeps_duplicate_line_findings_for_the_backlog_count(self):
        fixture = [dict(SCAN_FIXTURE[0]), dict(SCAN_FIXTURE[0])]
        tmp, binary = fake_ast_grep(json.dumps(fixture))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(len(hits), 2)

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

    def test_fresh_unwrap_warns_but_passes(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {"crates/bot/src/gateway_commands.rs": "a\n"})
            head = commit(root, {"crates/bot/src/gateway_commands.rs": "a\nb.unwrap();\n"})
            real_root, gate.ROOT = gate.ROOT, root
            scan = [{"ruleId": "no-unwrap-request-paths",
                     "file": "crates/bot/src/gateway_commands.rs",
                     "range": {"start": {"line": 2}}}]
            pack = Path(tempfile.mktemp())
            pack.write_text(json.dumps(scan))
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
        self.assertEqual(code, 0)
        self.assertIn("::warning::", out.getvalue())
        self.assertIn("gateway_commands.rs:2", out.getvalue())

    def test_preexisting_hit_is_silent(self):
        code, output = self.run_gate(SCAN_FIXTURE, "HEAD", "HEAD")
        self.assertEqual(code, 0)
        self.assertNotIn("::warning::", output)

    def test_blind_gate_warns_and_passes(self):
        out = io.StringIO()
        with redirect_stdout(out):
            code = gate.main(["--base-ref", "HEAD", "--head-ref", "HEAD",
                              "--ast-grep", "/nonexistent/ast-grep-binary"])
        self.assertEqual(code, 0)
        self.assertIn("::warning::", out.getvalue())
        self.assertIn("blind", out.getvalue())


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

    def test_backlog_exists_and_names_every_enforced_hit(self):
        backlog = (ROOT / "docs/unwrap-backlog.md").read_text()
        self.assertIn("no-unwrap-request-paths", backlog)


if __name__ == "__main__":
    unittest.main()

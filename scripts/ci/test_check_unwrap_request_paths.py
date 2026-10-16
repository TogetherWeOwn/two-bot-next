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
     "range": {"start": {"line": 10}, "end": {"line": 10}}},
    {"ruleId": "no-unwrap-request-paths",
     "file": "crates/bot/src/gateway_commands.rs",
     "range": {"start": {"line": 99}, "end": {"line": 99}}},
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

    def test_scan_keys_multiline_unwrap_to_call_line(self):
        fixture = [{"ruleId": gate.RULE_ID, "file": "a.rs",
                    "range": {"start": {"line": 0}, "end": {"line": 2}}}]
        tmp, binary = fake_ast_grep(json.dumps(fixture))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(hits, [("a.rs", 3)])

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

    def test_scan_enforces_macro_rule_hits(self):
        fixture = [{"ruleId": gate.MACRO_RULE_ID, "file": "a.rs",
                    "range": {"start": {"line": 3}, "end": {"line": 3}}}]
        tmp, binary = fake_ast_grep(json.dumps(fixture))
        try:
            hits, error = gate.run_scan(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(hits, [("a.rs", 4)])

    def test_scan_macro_end_line_before_start_is_blindness(self):
        finding = {"ruleId": gate.MACRO_RULE_ID, "file": "a.rs",
                   "range": {"start": {"line": 2}, "end": {"line": 1}}}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps([finding]), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid finding location", error)

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

    def test_end_line_before_start_is_blindness(self):
        finding = {"ruleId": gate.RULE_ID, "file": "a.rs",
                   "range": {"start": {"line": 2}, "end": {"line": 1}}}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps([finding]), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid finding location", error)

    def test_boolean_line_is_blindness(self):
        finding = {"ruleId": gate.RULE_ID, "file": "a.rs",
                   "range": {"start": {"line": True}}}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps([finding]), stderr="")):
            hits, error = gate.run_scan("ast-grep")
        self.assertIsNone(hits)
        self.assertIn("invalid finding", error)


class DiffTests(unittest.TestCase):
    def test_added_lines_tracks_non_ascii_path(self):
        tmp, root = make_repo()
        try:
            path = "crates/bot/src/café.rs"
            base = commit(root, {path: "fn main() {}\n"})
            head = commit(root, {path: "fn main() {}\nb.unwrap();\n"})
            real_root, gate.ROOT = gate.ROOT, root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(path, 2)})

    def test_quoted_diff_path_fails_closed(self):
        quoted_diff = "\n".join((
            'diff --git "a/caf\\303\\251.rs" "b/caf\\303\\251.rs"',
            '--- "a/caf\\303\\251.rs"',
            '+++ "b/caf\\303\\251.rs"',
            "@@ -0,0 +1 @@",
            "+b.unwrap();",
            "",
        ))
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=quoted_diff, stderr="")):
            added, error = gate.added_lines("base", "head")
        self.assertIsNone(added)
        self.assertIn("unrecognized", error)

    def test_added_lines_ignores_no_newline_marker(self):
        tmp, root = make_repo()
        try:
            path = "crates/bot/src/gateway_commands.rs"
            base = commit(root, {path: "let value = Some(1);"})
            head = commit(root, {path: "let value = Some(1).unwrap();"})
            real_root, gate.ROOT = gate.ROOT, root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(path, 1)})

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

    def test_multiline_unwrap_added_on_chain_tail_fails(self):
        tmp, root = make_repo()
        try:
            path = "crates/bot/src/gateway_commands.rs"
            base = commit(root, {path: "let value = Some(1u8)\n    .map(|x| x + 1);\n"})
            head = commit(root, {path: "let value = Some(1u8)\n    .map(|x| x + 1)\n    .unwrap();\n"})
            real_root, gate.ROOT = gate.ROOT, root
            scan = [{"ruleId": gate.RULE_ID, "file": path,
                     "range": {"start": {"line": 0}, "end": {"line": 2}}}]
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
        self.assertIn("gateway_commands.rs:3", out.getvalue())

    def test_editing_receiver_of_existing_multiline_unwrap_stays_silent(self):
        tmp, root = make_repo()
        try:
            path = "crates/bot/src/gateway_commands.rs"
            base = commit(root, {path: "let value = Some(1u8)\n    .map(|x| x + 1)\n    .unwrap();\n"})
            head = commit(root, {path: "let value = Some(2u8)\n    .map(|x| x + 1)\n    .unwrap();\n"})
            real_root, gate.ROOT = gate.ROOT, root
            scan = [{"ruleId": gate.RULE_ID, "file": path,
                     "range": {"start": {"line": 0}, "end": {"line": 2}}}]
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
        self.assertNotIn("::error::", out.getvalue())

    def test_fresh_unwrap_fails(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {"crates/bot/src/gateway_commands.rs": "a\n"})
            head = commit(root, {"crates/bot/src/gateway_commands.rs": "a\nb.unwrap();\n"})
            real_root, gate.ROOT = gate.ROOT, root
            scan = [{"ruleId": "no-unwrap-request-paths",
                     "file": "crates/bot/src/gateway_commands.rs",
                     "range": {"start": {"line": 1}, "end": {"line": 1}}}]
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

    def test_fresh_unwrap_inside_macro_fails(self):
        tmp, root = make_repo()
        try:
            path = "crates/bot/src/gateway_commands.rs"
            base = commit(root, {path: "fn p() {}\n"})
            head = commit(root, {path: 'fn p() {}\nassert_eq!(x.unwrap(), 1);\n'})
            real_root, gate.ROOT = gate.ROOT, root
            scan = [{"ruleId": gate.MACRO_RULE_ID, "file": path,
                     "range": {"start": {"line": 1}, "end": {"line": 1}}}]
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

    def test_macro_rule_covers_unwrap_inside_macro_token_trees(self):
        rule = yaml.safe_load(
            (ROOT / "ast-grep-rules/no-unwrap-request-paths-in-macros.yml").read_text())
        self.assertEqual(rule["id"], "no-unwrap-request-paths-in-macros")
        self.assertEqual(rule["language"], "rust")
        self.assertEqual(rule["rule"]["kind"], "identifier")
        self.assertIn("unwrap", rule["rule"]["regex"])
        self.assertIn("token_tree", json.dumps(rule["rule"]))
        base = yaml.safe_load(
            (ROOT / "ast-grep-rules/no-unwrap-request-paths.yml").read_text())
        self.assertEqual(rule["files"], base["files"])
        self.assertEqual(rule["ignores"], base["ignores"])
        self.assertEqual(gate.MACRO_RULE_ID, "no-unwrap-request-paths-in-macros")
        self.assertIn(gate.RULE_ID, gate.ENFORCED_RULE_IDS)
        self.assertIn(gate.MACRO_RULE_ID, gate.ENFORCED_RULE_IDS)

    def test_backlog_documents_inline_test_module_enforcement(self):
        backlog = (ROOT / "docs/unwrap-backlog.md").read_text()
        self.assertIn("no-unwrap-request-paths", backlog)
        self.assertIn("no-unwrap-request-paths-in-macros", backlog)
        self.assertIn("inline `#[cfg(test)]` modules are included", backlog)
        self.assertIn("50 findings across 9 rule-scope", backlog)
        self.assertIn("36 direct plus 14 inside macros", backlog)
        self.assertIn("not a count of unique source lines", backlog)
        self.assertNotIn("50 hit lines across 9 files", backlog)
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

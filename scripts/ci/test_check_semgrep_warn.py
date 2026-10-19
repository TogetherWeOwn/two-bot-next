"""Offline tests for the Semgrep HARD gate.

The gate must fail on new findings, stay silent when the diff adds none,
and fail closed when it is blind. The vendored rule set is pinned: five
rules, local config only, no registry access.
"""

import io
import json
import re
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
    "semgrep_gate", Path(__file__).with_name("check-semgrep-warn.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


REPORT = {"results": [
    {"check_id": "rust-warn.rust-panic-in-prod", "path": "crates/core/src/x.rs",
     "start": {"line": 4}},
    {"check_id": "rust-warn.rust-todo-unimplemented", "path": "crates/core/src/x.rs",
     "start": {"line": 9}},
    {"check_id": "rust-warn.rust-panic-in-prod", "path": "crates/core/src/x.rs",
     "start": {"line": 20}},
]}


# Verbatim `errors[]` entry from semgrep 1.180.0 (the pinned version) for a
# file with a recoverable syntax error: `type` is a [name, payload] list.
REAL_PARTIAL_PARSING_ERROR = {
    "code": 3,
    "level": "warn",
    "type": ["PartialParsing", [{
        "path": "crates/x/src/bad.rs",
        "start": {"line": 1, "col": 24, "offset": 0},
        "end": {"line": 1, "col": 25, "offset": 1},
    }]],
    "message": "Syntax error at line crates/x/src/bad.rs:1:\n `2` was unexpected",
    "path": "crates/x/src/bad.rs",
    "spans": [{
        "file": "crates/x/src/bad.rs",
        "start": {"line": 1, "col": 24, "offset": 0},
        "end": {"line": 1, "col": 25, "offset": 1},
    }],
}

# Same semgrep version, file that fails to parse at all: `type` is a string.
REAL_SYNTAX_ERROR = {
    "code": 3,
    "level": "warn",
    "type": "Syntax error",
    "message": "Syntax error at line crates/x/src/bad.rs:1:\n `fn a( {` was unexpected",
    "path": "crates/x/src/bad.rs",
}


def fake_semgrep(payload):
    import shlex
    tmp = tempfile.TemporaryDirectory()
    fixture = Path(tmp.name) / "report.json"
    fixture.write_text(payload)
    binary = Path(tmp.name) / "semgrep"
    binary.write_text("#!/bin/sh\ncat %s\n" % shlex.quote(str(fixture)))
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return tmp, str(binary)


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


class ParseTests(unittest.TestCase):
    def test_findings_key_empty_expect_to_call_end_line(self):
        report = {"results": [{
            "check_id": "rust-warn.rust-empty-expect",
            "path": "crates/bot/src/logging.rs",
            "start": {"line": 10}, "end": {"line": 12},
        }]}
        findings, error = self.run_report(report)
        self.assertEqual(error, "")
        self.assertEqual(findings, {("rust-empty-expect", "crates/bot/src/logging.rs", 12)})

    def test_macro_empty_expect_keys_to_end_line(self):
        report = {"results": [{
            "check_id": "rust-warn.rust-empty-expect-in-macros",
            "path": "crates/bot/src/logging.rs",
            "start": {"line": 10}, "end": {"line": 12},
        }]}
        findings, error = self.run_report(report)
        self.assertEqual(error, "")
        self.assertEqual(
            findings, {("rust-empty-expect-in-macros", "crates/bot/src/logging.rs", 12)})

    def test_macro_empty_expect_end_line_before_start_is_blindness(self):
        report = {"results": [{
            "check_id": "rust-warn.rust-empty-expect-in-macros", "path": "a.rs",
            "start": {"line": 2}, "end": {"line": 1},
        }]}
        findings, error = self.run_report(report)
        self.assertIsNone(findings)
        self.assertIn("invalid result location", error)

    def test_macro_duplicate_of_base_collapses_to_base(self):
        report = {"results": [
            {"check_id": "rust-warn.rust-empty-expect",
             "path": "crates/bot/src/logging.rs",
             "start": {"line": 10}, "end": {"line": 12}},
            {"check_id": "rust-warn.rust-empty-expect-in-macros",
             "path": "crates/bot/src/logging.rs",
             "start": {"line": 10}, "end": {"line": 12}},
        ]}
        findings, error = self.run_report(report)
        self.assertEqual(error, "")
        self.assertEqual(
            findings, {("rust-empty-expect", "crates/bot/src/logging.rs", 12)})

    def test_findings_keep_rule_path_and_line(self):
        tmp, binary = fake_semgrep(json.dumps(REPORT))
        try:
            findings, error = gate.run_semgrep(binary)
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(findings, {("rust-panic-in-prod", "crates/core/src/x.rs", 4),
                                    ("rust-todo-unimplemented", "crates/core/src/x.rs", 9),
                                    ("rust-panic-in-prod", "crates/core/src/x.rs", 20)})

    def test_garbage_report_is_blindness(self):
        tmp, binary = fake_semgrep("not json")
        try:
            findings, error = gate.run_semgrep(binary)
        finally:
            tmp.cleanup()
        self.assertIsNone(findings)
        self.assertTrue(error)

    def test_missing_binary_is_blindness(self):
        findings, error = gate.run_semgrep("/nonexistent/semgrep")
        self.assertIsNone(findings)
        self.assertTrue(error)

    def test_nonzero_exit_with_empty_output_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=2, stdout="", stderr="invalid config")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("exited 2", error)

    def test_warning_error_entry_keeps_findings_and_reports_details(self):
        report = {"results": REPORT["results"], "errors": [{
            "level": "warn", "type": "DependencyResolutionError", "path": "crates/core/src/y.rs",
            "message": "dependency resolution was unavailable",
        }]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, warning = gate.run_semgrep("semgrep")
        self.assertIsNotNone(findings)
        self.assertEqual(len(findings), 3)
        self.assertIn("DependencyResolutionError", warning)
        self.assertIn("crates/core/src/y.rs", warning)
        self.assertIn("dependency resolution was unavailable", warning)

    def test_warn_level_scan_failures_are_blindness_with_details(self):
        failure_types = (
            "Timeout", "OutOfMemory", "StackOverflow", "FixpointTimeout",
            "TimeoutDuringInterfile", "OutOfMemoryDuringInterfile",
            "ParseError", "PartialParsing", "PatternParseError", "RuleParseError",
        )
        for failure_type in failure_types:
            with self.subTest(failure_type=failure_type):
                report = {"results": [], "errors": [{
                    "level": "warn", "type": failure_type, "path": "crates/core/src/y.rs",
                    "message": "file could not be fully scanned",
                }]}
                with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                        returncode=0, stdout=json.dumps(report), stderr="")):
                    findings, error = gate.run_semgrep("semgrep")
                self.assertIsNone(findings)
                self.assertIn(failure_type, error)
                self.assertIn("crates/core/src/y.rs", error)

    def run_report(self, report):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            return gate.run_semgrep("semgrep")

    def test_real_partial_parsing_entry_is_blindness_with_details(self):
        findings, error = self.run_report(
            {"results": REPORT["results"], "errors": [REAL_PARTIAL_PARSING_ERROR]})
        self.assertIsNone(findings)
        self.assertIn("type=PartialParsing", error)
        self.assertIn("crates/x/src/bad.rs", error)
        self.assertIn("was unexpected", error)
        self.assertNotIn("invalid error entry", error)

    def test_real_syntax_error_entry_is_blindness_with_details(self):
        findings, error = self.run_report({"results": [], "errors": [REAL_SYNTAX_ERROR]})
        self.assertIsNone(findings)
        self.assertIn("type=Syntax error", error)
        self.assertIn("crates/x/src/bad.rs", error)

    def test_list_typed_nonfatal_warning_keeps_findings(self):
        entry = {"level": "warn", "path": "crates/core/src/y.rs",
                 "type": ["DependencyResolutionError", ["lockfile unreadable"]],
                 "message": "dependency resolution was unavailable"}
        findings, warning = self.run_report({"results": REPORT["results"], "errors": [entry]})
        self.assertEqual(len(findings), 3)
        self.assertIn("type=DependencyResolutionError", warning)

    def test_warn_level_config_error_is_blindness(self):
        entry = {"level": "warn", "type": "Config error", "code": 2,
                 "path": "config.yml", "message": "invalid rule"}
        findings, error = self.run_report({"results": [], "errors": [entry]})
        self.assertIsNone(findings)
        self.assertIn("Config error", error)

    def test_unknown_level_is_blindness(self):
        entry = {"level": "fatal", "type": "DependencyResolutionError",
                 "path": "crates/core/src/y.rs", "message": "unexpected severity"}
        findings, error = self.run_report({"results": [], "errors": [entry]})
        self.assertIsNone(findings)
        self.assertIn("level=fatal", error)

    def test_malformed_error_type_is_blindness(self):
        for bad_type in ([], [1, "x"], 5, None, {"name": "PartialParsing"}):
            with self.subTest(bad_type=bad_type):
                entry = {"level": "warn", "type": bad_type, "path": "a.rs", "message": "m"}
                findings, error = self.run_report({"results": [], "errors": [entry]})
                self.assertIsNone(findings)
                self.assertIn("invalid error entry", error)

    def test_scan_disables_per_file_timeout(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps({"results": []}), stderr="")) as run:
            gate.run_semgrep("semgrep")
        argv = run.call_args.args[0]
        self.assertEqual(argv[argv.index("--timeout") + 1], "0")

    def test_error_level_entry_is_blindness_with_details(self):
        report = {"results": [], "errors": [{
            "level": "error", "type": "Config error", "path": "config.yml",
            "message": "invalid rule",
        }]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("Config error", error)
        self.assertIn("config.yml", error)
        self.assertIn("invalid rule", error)

    def test_empty_success_output_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout="", stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("empty", error)

    def test_malformed_result_is_blindness(self):
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps({"results": [None]}), stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("invalid result", error)

    def test_malformed_result_location_is_blindness(self):
        report = {"results": [{"check_id": "rule", "path": "a.rs", "start": "bad"}]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("invalid result", error)

    def test_result_without_line_is_blindness(self):
        report = {"results": [{"check_id": "rule", "path": "a.rs", "start": {}}]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("invalid result", error)

    def test_empty_expect_end_line_before_start_is_blindness(self):
        report = {"results": [{
            "check_id": "rust-warn.rust-empty-expect", "path": "a.rs",
            "start": {"line": 2}, "end": {"line": 1},
        }]}
        findings, error = self.run_report(report)
        self.assertIsNone(findings)
        self.assertIn("invalid result location", error)

    def test_boolean_line_is_blindness(self):
        report = {"results": [{"check_id": "rule", "path": "a.rs", "start": {"line": True}}]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("invalid result", error)


class DiffTests(unittest.TestCase):
    def test_added_lines_tracks_non_ascii_path(self):
        tmp, root = make_repo()
        try:
            path = "crates/core/src/café.rs"
            base = commit(root, {path: "fn main() {}\n"})
            head = commit(root, {path: "fn main() {}\npanic!(\"new\");\n"})
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
            '+panic!("new");',
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
            path = "crates/core/src/example.rs"
            base = commit(root, {path: "fn main() {}"})
            head = commit(root, {path: 'fn main() { panic!("new"); }'})
            real_root, gate.ROOT = gate.ROOT, root
            try:
                added, error = gate.added_lines(base, head)
            finally:
                gate.ROOT = real_root
        finally:
            tmp.cleanup()
        self.assertEqual(error, "")
        self.assertEqual(added, {(path, 1)})

    def test_pure_rename_does_not_add_every_line(self):
        tmp, root = make_repo()
        try:
            base = commit(root, {"logging.rs": "one\ntwo\nthree\n"})
            (root / "logging.rs").rename(root / "logging_moved.rs")
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


class GateTests(unittest.TestCase):
    def run_gate(self, report, added):
        tmp, binary = fake_semgrep(json.dumps(report))
        real_run, real_added = gate.run_semgrep, gate.added_lines
        gate.run_semgrep = lambda _binary: real_run(binary)
        gate.added_lines = lambda _base, _head: (set(added), "")
        out = io.StringIO()
        try:
            with redirect_stdout(out):
                code = gate.main(["--semgrep", binary])
        finally:
            gate.run_semgrep, gate.added_lines = real_run, real_added
            tmp.cleanup()
        return code, out.getvalue()

    def test_multiline_expect_added_on_chain_tail_fails(self):
        report = {"results": [{
            "check_id": "rust-warn.rust-empty-expect",
            "path": "crates/bot/src/logging.rs",
            "start": {"line": 1032}, "end": {"line": 1034},
        }]}
        code, output = self.run_gate(
            report, {("crates/bot/src/logging.rs", 1034)})
        self.assertEqual(code, 1)
        self.assertIn("logging.rs:1034", output)

    def test_editing_receiver_of_existing_multiline_expect_stays_silent(self):
        report = {"results": [{
            "check_id": "rust-warn.rust-empty-expect",
            "path": "crates/bot/src/logging.rs",
            "start": {"line": 1032}, "end": {"line": 1034},
        }]}
        code, output = self.run_gate(
            report, {("crates/bot/src/logging.rs", 1032)})
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)

    def test_new_finding_fails(self):
        code, output = self.run_gate(REPORT, {("crates/core/src/x.rs", 4)})
        self.assertEqual(code, 1)
        self.assertIn("::error::", output)
        self.assertIn("rust-panic-in-prod", output)

    def test_old_finding_is_silent(self):
        code, output = self.run_gate(REPORT, {("crates/core/src/x.rs", 100)})
        self.assertEqual(code, 0)
        self.assertNotIn("::error::", output)
        self.assertNotIn("::warning::", output)

    def test_dependency_warning_is_annotated_but_does_not_fail(self):
        report = {"results": [], "errors": [{
            "level": "warn", "type": "DependencyResolutionError", "path": "crates/core/src/y.rs",
            "message": "dependency resolution was unavailable",
        }]}
        code, output = self.run_gate(report, set())
        self.assertEqual(code, 0)
        self.assertIn("::warning::", output)
        self.assertIn("DependencyResolutionError", output)
        self.assertIn("crates/core/src/y.rs", output)
        self.assertIn("dependency resolution was unavailable", output)

    def test_warning_details_cannot_inject_workflow_commands(self):
        report = {"results": [], "errors": [{
            "level": "warn", "type": "DependencyResolutionError", "path": "file.rs",
            "message": "dependency warning\n::error::injected",
        }]}
        code, output = self.run_gate(report, set())
        self.assertEqual(code, 0)
        self.assertEqual(output.count("::warning::"), 1)
        self.assertFalse(any(line.startswith("::error::") for line in output.splitlines()))

    def test_blind_gate_fails_closed(self):
        out = io.StringIO()
        real_added = gate.added_lines
        gate.added_lines = lambda _base, _head: (set(), "")
        try:
            with redirect_stdout(out):
                code = gate.main(["--semgrep", "/nonexistent/semgrep"])
        finally:
            gate.added_lines = real_added
        self.assertEqual(code, 1)
        self.assertIn("::error::", out.getvalue())
        self.assertIn("blind", out.getvalue())


class ConfigTests(unittest.TestCase):
    EXPECTED_RULES = {"rust-todo-unimplemented", "rust-empty-expect",
                      "rust-empty-expect-in-macros",
                      "rust-panic-in-prod", "rust-process-exit-in-lib"}

    def test_rule_set_is_pinned_and_local(self):
        config = yaml.safe_load((ROOT / ".semgrep/rust-warn.yml").read_text())
        self.assertEqual({rule["id"] for rule in config["rules"]}, self.EXPECTED_RULES)
        for rule in config["rules"]:
            self.assertEqual(rule["languages"], ["rust"])
            self.assertEqual(rule["severity"], "WARNING")
            self.assertIn("paths", rule)
            text = json.dumps(rule)
            self.assertNotIn("registry", text)
            self.assertNotIn("p/rust", text)

    def test_macro_rule_excludes_line_comments(self):
        config = yaml.safe_load((ROOT / ".semgrep/rust-warn.yml").read_text())
        rule = next(r for r in config["rules"]
                    if r["id"] == "rust-empty-expect-in-macros")
        patterns = rule.get("patterns", [])
        regexes = [p["pattern-regex"] for p in patterns if "pattern-regex" in p]
        self.assertTrue(regexes, "macro rule must keep its text regex")
        self.assertTrue(any(re.search(rx, 'x.expect("")') for rx in regexes))
        not_regexes = [p["pattern-not-regex"] for p in patterns
                       if "pattern-not-regex" in p]
        self.assertTrue(not_regexes, "macro rule must exclude // comments")
        self.assertTrue(any(
            re.search(rx, "/// Never write .expect(\"\")", re.M)
            for rx in not_regexes))
        self.assertTrue(any(
            re.search(rx, '    // indented .expect("")', re.M)
            for rx in not_regexes))
        code_line = 'format!("https://{}/x", host.expect(""))'
        expect_at = code_line.index('.expect("")')
        self.assertTrue(any(re.search(rx, code_line) for rx in regexes))
        for rx in not_regexes:
            for m in re.finditer(rx, code_line, re.M):
                self.assertFalse(
                    m.start() <= expect_at < m.end(),
                    f"exclusion {rx!r} must not swallow a real hit after //",
                )

    def test_test_files_are_out_of_scope(self):
        config = yaml.safe_load((ROOT / ".semgrep/rust-warn.yml").read_text())
        for rule in config["rules"]:
            exclude = " ".join(rule["paths"].get("exclude", []))
            self.assertIn("_tests", exclude)


if __name__ == "__main__":
    unittest.main()

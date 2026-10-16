"""Offline tests for the Semgrep HARD gate.

The gate must fail on new findings, stay silent when the diff adds none,
and fail closed when it is blind. The vendored rule set is pinned: four
rules, local config only, no registry access.
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
            "level": "warn", "type": "Timeout", "path": "crates/core/src/y.rs",
            "message": "scan reached per-file timeout",
        }]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, warning = gate.run_semgrep("semgrep")
        self.assertIsNotNone(findings)
        self.assertEqual(len(findings), 3)
        self.assertIn("Timeout", warning)
        self.assertIn("crates/core/src/y.rs", warning)
        self.assertIn("scan reached per-file timeout", warning)

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

    def test_boolean_line_is_blindness(self):
        report = {"results": [{"check_id": "rule", "path": "a.rs", "start": {"line": True}}]}
        with patch.object(gate.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(report), stderr="")):
            findings, error = gate.run_semgrep("semgrep")
        self.assertIsNone(findings)
        self.assertIn("invalid result", error)


class DiffTests(unittest.TestCase):
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

    def test_warn_level_scan_error_is_annotated_but_does_not_fail(self):
        report = {"results": [], "errors": [{
            "level": "warn", "type": "Timeout", "path": "crates/core/src/y.rs",
            "message": "scan reached per-file timeout",
        }]}
        code, output = self.run_gate(report, set())
        self.assertEqual(code, 0)
        self.assertIn("::warning::", output)
        self.assertIn("Timeout", output)
        self.assertIn("crates/core/src/y.rs", output)
        self.assertIn("scan reached per-file timeout", output)

    def test_warning_details_cannot_inject_workflow_commands(self):
        report = {"results": [], "errors": [{
            "level": "warn", "type": "Timeout", "path": "file.rs",
            "message": "timeout\n::error::injected",
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

    def test_test_files_are_out_of_scope(self):
        config = yaml.safe_load((ROOT / ".semgrep/rust-warn.yml").read_text())
        for rule in config["rules"]:
            exclude = " ".join(rule["paths"].get("exclude", []))
            self.assertIn("_tests", exclude)


if __name__ == "__main__":
    unittest.main()

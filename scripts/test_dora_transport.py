"""Credentialless contract tests for the DORA read-only subprocess transport.

Only subprocess boundaries are mocked. Literal shell-looking inputs also cross
an actual local fake-gh process; neither fixture can reach GitHub or read auth.
"""

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import traceback
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dora_metrics


ENDPOINT = "repos/fixture/dora/deployments"
PRIVATE_TEXT = "transport-private-fixture-" * 1024


class TransportBoundaryTests(unittest.TestCase):
    def invoke(self, path=ENDPOINT, **fields):
        result = subprocess.CompletedProcess([], 0, '{"ok": true}', "")
        with patch.object(dora_metrics.subprocess, "run", return_value=result) as run:
            self.assertEqual(dora_metrics.gh_api_get(path, **fields), {"ok": True})
        run.assert_called_once()
        return run.call_args

    def test_no_fields_uses_fixed_gh_api_get_and_delimited_endpoint(self):
        invocation = self.invoke()
        self.assertEqual(invocation.args, (
            ["gh", "api", "--method", "GET", "--", ENDPOINT],
        ))

    def test_multiple_fields_are_separate_pairs_before_endpoint_delimiter(self):
        invocation = self.invoke(environment="production", per_page=100, page=2)
        self.assertEqual(invocation.args, (
            ["gh", "api", "--method", "GET", "-f", "environment=production",
             "-f", "per_page=100", "-f", "page=2", "--", ENDPOINT],
        ))

    def test_shell_false_is_explicit_not_an_implicit_default(self):
        invocation = self.invoke()
        self.assertIs(invocation.kwargs.get("shell"), False)

    def test_timeout_and_captured_text_are_explicit(self):
        invocation = self.invoke()
        self.assertEqual(invocation.kwargs.get("timeout"), 30)
        self.assertIs(invocation.kwargs.get("capture_output"), True)
        self.assertIs(invocation.kwargs.get("text"), True)

    def test_option_looking_endpoints_cannot_become_cli_flags(self):
        for endpoint in ("--method=POST", "-XDELETE", "--hostname=fixture.invalid",
                         "--input=fixture.json", "--help"):
            with self.subTest(endpoint=endpoint):
                invocation = self.invoke(endpoint, page=1)
                self.assertEqual(invocation.args, (
                    ["gh", "api", "--method", "GET", "-f", "page=1",
                     "--", endpoint],
                ))

    def test_field_keys_and_values_are_not_split_or_promoted_to_options(self):
        fields = {"--method": "DELETE", "a=b": "one=two three\n雪",
                  "query": "--hostname=fixture.invalid; $(printf literal)"}
        invocation = self.invoke(**fields)
        argv = invocation.args[0]
        # Delimiter placement is tested separately, so this is a baseline control.
        for key, value in fields.items():
            pair_at = argv.index(f"{key}={value}")
            self.assertEqual(argv[pair_at - 1:pair_at + 1], ["-f", f"{key}={value}"])
        self.assertEqual(len(argv), 5 + 2 * len(fields) + argv.count("--"))

    def test_valid_json_keeps_objects_arrays_and_scalars_unchanged(self):
        for value in ({"nested": [True, None, {"雪": "a=b; $(literal)"}]},
                      [], [0, False], "text", 42, 1.25, False, None):
            with self.subTest(value=value):
                result = subprocess.CompletedProcess([], 0, json.dumps(value), PRIVATE_TEXT)
                out, err = io.StringIO(), io.StringIO()
                with patch.object(dora_metrics.subprocess, "run", return_value=result), \
                        redirect_stdout(out), redirect_stderr(err):
                    decoded = dora_metrics.gh_api_get(ENDPOINT)
                self.assertEqual(decoded, value)
                self.assertIs(type(decoded), type(value))
                self.assertEqual((out.getvalue(), err.getvalue()), ("", ""))


class TransportDiagnosticTests(unittest.TestCase):
    def assert_fixed_error(self, expected, *, result=None, exception=None):
        out, err = io.StringIO(), io.StringIO()
        with patch.object(dora_metrics.subprocess, "run", return_value=result,
                          side_effect=exception) as run, \
                redirect_stdout(out), redirect_stderr(err):
            try:
                dora_metrics.gh_api_get(ENDPOINT, query=PRIVATE_TEXT)
            except dora_metrics.DoraError as caught:
                diagnostic = str(caught)
                rendered = "".join(traceback.format_exception(caught))
            else:
                self.fail("transport must refuse with DoraError")
        run.assert_called_once()
        self.assertEqual(diagnostic, expected)
        self.assertLessEqual(len(diagnostic), 64)
        self.assertEqual((out.getvalue(), err.getvalue()), ("", ""))
        self.assertNotIn(PRIVATE_TEXT, rendered)

    def test_os_error_is_fixed_and_hides_exception_content(self):
        self.assert_fixed_error("github api unavailable", exception=OSError(PRIVATE_TEXT))

    def test_timeout_is_fixed_and_hides_command_stdout_and_stderr(self):
        exception = subprocess.TimeoutExpired(
            ["gh", PRIVATE_TEXT], 30, output=PRIVATE_TEXT, stderr=PRIVATE_TEXT,
        )
        self.assert_fixed_error("github api unavailable", exception=exception)

    def test_subprocess_error_is_fixed_and_hides_exception_content(self):
        self.assert_fixed_error("github api unavailable",
                                exception=subprocess.SubprocessError(PRIVATE_TEXT))

    def test_called_process_error_does_not_expose_command_or_captured_output(self):
        exception = subprocess.CalledProcessError(
            17, ["gh", PRIVATE_TEXT], output=PRIVATE_TEXT, stderr=PRIVATE_TEXT,
        )
        self.assert_fixed_error("github api unavailable", exception=exception)

    def test_nonzero_exit_refuses_even_valid_json_without_body_or_stderr(self):
        result = subprocess.CompletedProcess([], 17, json.dumps(PRIVATE_TEXT), PRIVATE_TEXT)
        self.assert_fixed_error("github api refused the read", result=result)

    def test_malformed_json_is_fixed_without_body_stderr_or_parser_leakage(self):
        for body in ("", "not JSON " + PRIVATE_TEXT, '{"private":"' + PRIVATE_TEXT,
                     json.dumps(PRIVATE_TEXT) + " trailing text"):
            with self.subTest(body_length=len(body)):
                result = subprocess.CompletedProcess([], 0, body, PRIVATE_TEXT)
                self.assert_fixed_error("github api returned invalid JSON", result=result)


class LiteralChildProcessTests(unittest.TestCase):
    def test_shell_looking_endpoint_and_fields_roundtrip_without_execution(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        with tempfile.TemporaryDirectory(prefix="dora-transport-", dir=scratch) as directory:
            root = Path(directory)
            fake_gh = root / "gh"
            fake_gh.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "print(json.dumps({'argv': sys.argv[1:], 'auth_present': any(\n"
                "    key in os.environ for key in ('GH_TOKEN', 'GITHUB_TOKEN', 'GH_CONFIG_DIR')\n"
                ")}))\n"
                "print('local fake-gh stderr', file=sys.stderr)\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o700)
            home = root / "home"
            home.mkdir()
            for syntax in ("semicolon", "substitution"):
                for location in ("endpoint", "field"):
                    with self.subTest(syntax=syntax, location=location):
                        marker = root / f"executed-{syntax}-{location}"
                        # printf is a shell builtin: detection needs no external touch binary.
                        payload = (f"; printf executed > {marker}" if syntax == "semicolon"
                                   else f"$(printf executed > {marker})")
                        endpoint = ENDPOINT + payload if location == "endpoint" else ENDPOINT
                        fields = {"query": "literal " + payload} if location == "field" else {}
                        out, err = io.StringIO(), io.StringIO()
                        # Only the local fake executable is on PATH; no inherited auth/config.
                        with patch.dict(os.environ, {"PATH": str(root), "HOME": str(home)},
                                        clear=True), redirect_stdout(out), redirect_stderr(err):
                            decoded = dora_metrics.gh_api_get(endpoint, **fields)
                        self.assertFalse(marker.exists(), "literal input executed a shell command")
                        self.assertFalse(decoded["auth_present"])
                        argv = decoded["argv"]
                        self.assertEqual(argv[:3], ["api", "--method", "GET"])
                        self.assertIn(endpoint, argv)
                        for key, value in fields.items():
                            pair_at = argv.index(f"{key}={value}")
                            self.assertEqual(argv[pair_at - 1:pair_at + 1],
                                             ["-f", f"{key}={value}"])
                        self.assertEqual(len(argv), 4 + 2 * len(fields) + argv.count("--"))
                        self.assertEqual((out.getvalue(), err.getvalue()), ("", ""))


if __name__ == "__main__":
    unittest.main()

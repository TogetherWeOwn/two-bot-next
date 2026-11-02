"""Protected acceptance for bounded, redacted cutover probe verdicts.

Run with stdlib unittest; all HTTP is replaced below the real grading boundary.
No sockets, databases, credentials, implementation diffs or source-text pins.
The paired implementation must retain this file unchanged. See the adjacent
JSON manifest for the handoff and the deliberately small machine contract.
"""

import contextlib
import importlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import urllib.request
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
probe = importlib.import_module("qa_cutover_probes")

ORIGIN = "https://probe.example"
SHA = "0123456789abcdef0123456789abcdef01234567"
BUILD_ID = "700-1"
CHECK_IDS = {"liveness", "readiness-shape", "gateway-state", "jobs-map", "fence-watch"}
SENTINEL = "REMOTE_SENTINEL_cutover_do_not_print"
# Synthetic only, constructed to exercise token-shaped diagnostic input.
TOKEN = "ghp_" + "Q" * 36
NOW = 1_800_000_000
OUTPUT_BOUND = 16_384


def ready_payload():
    return {
        "components": [["process", "ready"], ["gateway", "ready"],
                       ["database", "ready"], ["token_invalid", "ready"]],
        "jobs": {"counter": {"parked": False, "running": True,
                             "last_success": NOW * 1000,
                             "consecutive_failures": 0,
                             "last_error_class": None}},
        "build_revision": SHA,
        "build_id": BUILD_ID,
    }


def reply(payload, status=200):
    return status, json.dumps(payload).encode()


class CutoverVerdictAcceptance(unittest.TestCase):
    """Breaks caught: reflection, unbounded parsing, and false attestation."""

    def invoke(self, mapping=None, *flags, origin=ORIGIN):
        answers = {"/health": reply({"status": "ok"}),
                   "/readyz": reply(ready_payload())}
        if mapping:
            answers.update(mapping)
        calls = []

        def fetch(url):
            calls.append(url)
            for route, answer in answers.items():
                if url.endswith(route):
                    return answer
            self.fail("probe attempted an unexpected route")

        out, err = io.StringIO(), io.StringIO()
        # Guard against accidental real HTTP, including a regression ignoring
        # fetch_fn. Environment credential/origin fallbacks are not test inputs.
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch("socket.socket", side_effect=AssertionError("network forbidden")), \
                mock.patch.object(probe.time, "time", return_value=NOW), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = probe.main(["--base-url", origin, *flags], fetch_fn=fetch)
            except (Exception, SystemExit) as exc:
                self.fail(f"probe escaped verdict path: {type(exc).__name__}")
        self.assertIs(type(code), int)
        combined = out.getvalue() + err.getvalue()
        self.assertLessEqual(len(combined.encode()), OUTPUT_BOUND)
        self.assertNotIn("Traceback", combined)
        return code, out.getvalue(), err.getvalue(), calls

    def assert_redacted(self, output, *extra):
        for value in (SENTINEL, SENTINEL[:16], TOKEN, TOKEN[:12], *extra):
            self.assertTrue(value not in output, "remote input (or its truncated prefix) reached diagnostics")
        self.assertNotIn("\\u001b", output)
        self.assertNotIn("\\u0000", output)
        self.assertFalse(any(ord(char) < 32 and char not in "\n\t" for char in output))

    def machine(self, mapping=None, *flags, origin=ORIGIN):
        code, out, err, calls = self.invoke(mapping, "--json", *flags, origin=origin)
        self.assertEqual(err, "", "machine results must be one JSON document on stdout")
        try:
            report = json.loads(out)
        except (ValueError, RecursionError):
            self.fail("--json did not return a complete JSON verdict")
        self.assertIsInstance(report, dict)
        self.assertIn(report.get("verdict"), ("PASS", "FAIL"))
        self.assertEqual(code, 0 if report["verdict"] == "PASS" else 1)
        self.assertIsInstance(report.get("next_action"), str)
        self.assertTrue(report["next_action"].strip())
        self.assertLessEqual(len(report["next_action"]), 256)
        checks = report.get("checks")
        self.assertIsInstance(checks, list)
        self.assertTrue(checks)
        ids = []
        for check in checks:
            self.assertIsInstance(check, dict)
            ids.append(check.get("id"))
            self.assertRegex(check["id"], r"^[a-z][a-z0-9-]{0,63}$")
            self.assertIn(check.get("verdict"), ("PASS", "FAIL"))
            self.assertIsInstance(check.get("code"), str)
            self.assertRegex(check["code"], r"^[a-z][a-z0-9_-]{0,63}$")
            self.assertIsInstance(check.get("next_action"), str)
            self.assertTrue(check["next_action"].strip())
            self.assertLessEqual(len(check["next_action"]), 256)
        self.assertEqual(len(ids), len(set(ids)), "stable IDs must be unique")
        self.assertEqual(report["verdict"] == "PASS",
                         all(check["verdict"] == "PASS" for check in checks))
        # A failed verdict is not the existing success-only attestation schema.
        if code:
            self.assertNotEqual(report.get("result"), "pass")
        self.assert_redacted(json.dumps(report))
        return code, report, calls

    def test_invalid_origins_never_reflect_input_or_fetch(self):
        origins = (
            f"ftp://user:{TOKEN}@probe.example/",
            f"https://user:{TOKEN}@probe.example:bad/",
            f"ftp://user:{TOKEN}@probe.example:999999/",
            f"https://user:{TOKEN}@[broken/",
            f"https://probe.example/{SENTINEL}",
            f"https://probe.example?token={TOKEN}",
            f"https://probe.example#{SENTINEL}",
            f"https://{SENTINEL}:bad/",
            "https://", "https:///", "http://:8080", "https://@/",
            "https://probe.example:0", "https://probe.example:65536",
        )
        for origin in origins:
            with self.subTest(origin_kind=origins.index(origin)):
                code, out, err, calls = self.invoke(origin=origin)
                self.assert_redacted(out + err, origin)
                self.assertEqual(calls, [], "reject before fetching either route")
                self.assertEqual(code, 1)
                self.assertIn("FAIL", out)

    def test_origin_diagnostics_are_fixed_not_sanitized_remote_text(self):
        outputs = []
        for marker in ("UNTRUSTED_ALPHA", "UNTRUSTED_BETA"):
            code, out, err, calls = self.invoke(origin=f"ftp://user:{marker}@probe.example:bad/")
            self.assertEqual(code, 1)
            self.assertEqual(calls, [])
            self.assertNotIn(marker, out + err)
            outputs.append(out + err)
        self.assertEqual(outputs[0], outputs[1])

    def test_hostless_origin_is_not_admitted(self):
        code, _, _, calls = self.invoke(origin="https://:8443")
        self.assertEqual(calls, [])
        self.assertEqual(code, 1)

    def test_bare_origins_preserve_http_ports_ipv6_and_trailing_slash(self):
        # Exercise normalization through main, not a mirror of its parser.
        for origin, normalized in (("http://probe.example:8080/", "http://probe.example:8080"),
                                   ("https://[::1]:8443/", "https://[::1]:8443"),
                                   (ORIGIN + "/", ORIGIN)):
            calls = []

            def fetch(url):
                calls.append(url)
                return reply({"status": "ok"}) if url.endswith("/health") else reply(ready_payload())

            with mock.patch("socket.socket", side_effect=AssertionError("network forbidden")), \
                    mock.patch.object(probe.time, "time", return_value=NOW), \
                    contextlib.redirect_stdout(io.StringIO()):
                code = probe.main(["--base-url", origin], fetch_fn=fetch)
            self.assertEqual(code, 0)
            self.assertEqual(calls, [normalized + "/health", normalized + "/readyz"])

    def test_in_cap_deep_json_on_either_route_is_an_ordinary_failed_check(self):
        # Construct wire JSON without invoking the fixture encoder recursively.
        nested = b"[" * 2000 + b"0" + b"]" * 2000
        for route, check_id in (("/health", "liveness"), ("/readyz", "readiness-shape")):
            payload = {"status": "ok"} if route == "/health" else ready_payload()
            raw = json.dumps(payload).encode()[:-1] + b', "unused":' + nested + b"}"
            self.assertLess(len(raw), probe.BODY_CAP)
            with self.subTest(route=route):
                code, out, err, _ = self.invoke({route: (200, raw)})
                self.assertEqual(code, 1)
                self.assertIn(f"FAIL {check_id}:", out)
                self.assertEqual(err, "")

    def test_malformed_remote_shapes_are_failed_checks_not_exceptions(self):
        malformed = (None, [], True, 17, SENTINEL, {"components": None},
                     {**ready_payload(), "components": [["gateway"]]},
                     {**ready_payload(), "jobs": [SENTINEL]},
                     {**ready_payload(), "jobs": {"counter": SENTINEL}})
        for value in malformed:
            with self.subTest(shape=type(value).__name__):
                code, out, err, _ = self.invoke({"/readyz": reply(value)})
                self.assertEqual(code, 1)
                self.assert_redacted(out + err)
                self.assertIn("FAIL", out)

    def test_bad_build_fields_are_rejected_without_echo(self):
        for field in ("build_revision", "build_id"):
            for value in (SENTINEL, TOKEN, "line\nforged PASS", "escape\x1b[31m", "nul\x00tail",
                          "x" * 4096, "", [], {}, True, 42):
                with self.subTest(field=field, kind=type(value).__name__):
                    payload = ready_payload()
                    payload[field] = value
                    code, out, err, _ = self.invoke({"/readyz": reply(payload)})
                    self.assert_redacted(out + err, "forged PASS", "escape", "nul", "x" * 64)
                    self.assertEqual(code, 1)
                    self.assertIn("FAIL readiness-shape:", out)

    def test_build_contract_accepts_canonical_and_exact_unknown_pair(self):
        # Supported shared build_identity.py contract, not arbitrary strings.
        for revision, build_id in ((SHA, BUILD_ID), ("unknown", "unknown")):
            payload = ready_payload()
            payload.update(build_revision=revision, build_id=build_id)
            code, out, _, _ = self.invoke({"/readyz": reply(payload)})
            self.assertEqual(code, 0, out)

    def test_build_contract_refuses_mixed_unstamped_or_noncanonical_identity(self):
        for revision, build_id in (("unknown", BUILD_ID), (SHA, "unknown"),
                                   (SHA.upper(), BUILD_ID), (SHA, "7"),
                                   (SHA, "7-1\n"), (SHA, "1" * 21 + "-1")):
            with self.subTest(revision=revision, build_id=build_id):
                payload = ready_payload()
                payload.update(build_revision=revision, build_id=build_id)
                code, out, _, _ = self.invoke({"/readyz": reply(payload)})
                self.assertEqual(code, 1)
                self.assertIn("FAIL readiness-shape:", out)

    def test_untrusted_job_names_never_reach_diagnostics(self):
        for name in (SENTINEL, TOKEN, "job\nforged PASS", "job\x1b[31m", "j" * 4096):
            with self.subTest(name_kind=len(name)):
                payload = ready_payload()
                payload["jobs"] = {name: {"parked": False, "running": True,
                                          "last_success": (NOW - 1_000_000) * 1000}}
                _, out, err, _ = self.invoke({"/readyz": reply(payload)})
                self.assert_redacted(out + err, name, "forged PASS", "j" * 64)

    def test_unrecognized_job_remains_ungraded_without_echoing_its_name(self):
        payload = ready_payload()
        payload["jobs"] = {"brand_new_job": {"parked": False, "running": True,
                                              "last_success": 1}}
        code, out, err, _ = self.invoke({"/readyz": reply(payload)})
        self.assertEqual(code, 0, "do not invent a freshness policy for unknown jobs")
        self.assertNotIn("brand_new_job", out + err)
        self.assertIn("ungraded", out)

    def test_bad_error_classes_never_reach_failure_diagnostics(self):
        for value in (SENTINEL, TOKEN, "error\nforged PASS", "error\x1b[31m", "e" * 4096,
                      {"detail": SENTINEL}, [TOKEN], True, 19):
            with self.subTest(kind=type(value).__name__):
                payload = ready_payload()
                payload["jobs"]["counter"].update(last_success=None, consecutive_failures=4,
                                                 last_error_class=value)
                code, out, err, _ = self.invoke({"/readyz": reply(payload)})
                self.assert_redacted(out + err, "forged PASS", "e" * 64)
                self.assertEqual(code, 1, "actual job failures must remain red")
                self.assertIn("FAIL jobs-map:", out)

    def test_remote_fence_error_field_is_bounded_and_redacted(self):
        for error in (SENTINEL, TOKEN, "error\nforged PASS", {"detail": SENTINEL}, [TOKEN]):
            payload = {"error": error, "reason": SENTINEL}
            code, out, err, _ = self.invoke({"/readyz": reply(payload, 503)})
            self.assertEqual(code, 1)
            self.assert_redacted(out + err, "forged PASS")
            self.assertIn("FAIL fence-watch:", out)

    def test_supported_error_classes_keep_actual_job_failures_red(self):
        for error_class in ("database", "rest", "configuration", "timeout", "panic", "feed",
                            "recovery_required", None):
            payload = ready_payload()
            payload["jobs"]["counter"].update(last_success=None, consecutive_failures=4,
                                             last_error_class=error_class)
            code, out, _, _ = self.invoke({"/readyz": reply(payload)})
            self.assertEqual(code, 1)
            self.assertIn("FAIL jobs-map:", out)
            self.assertIn("counter", out)

    def test_many_unknown_jobs_cannot_expand_output_or_leak_fields(self):
        payload = ready_payload()
        payload["jobs"] = {f"{SENTINEL}_{i}": {"parked": False, "running": True}
                           for i in range(400)}
        self.assertLess(len(reply(payload)[1]), probe.BODY_CAP)
        _, out, err, _ = self.invoke({"/readyz": reply(payload)})
        self.assert_redacted(out + err)

    def test_unknown_fields_are_projected_out_of_success_and_evidence(self):
        payload = ready_payload()
        payload[SENTINEL] = TOKEN
        payload["jobs"]["counter"][SENTINEL] = TOKEN
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_SCRATCH_DIR")) as tmp:
            evidence = Path(tmp) / "success.json"
            code, out, err, _ = self.invoke({"/readyz": reply(payload)}, "--evidence", str(evidence))
            self.assertEqual(code, 0)
            self.assert_redacted(out + err + evidence.read_text())
            self.assertEqual(json.loads(evidence.read_text())["result"], "pass")

    def test_default_text_keeps_five_check_labels_and_pass_summary(self):
        code, out, err, calls = self.invoke()
        self.assertEqual(code, 0)
        self.assertEqual(err, "")
        self.assertEqual(calls, [ORIGIN + "/health", ORIGIN + "/readyz"])
        for check_id in CHECK_IDS:
            self.assertIn(f"PASS {check_id}:", out)
        self.assertIn("5/5 checks passed", out)

    def test_truthful_parked_is_green_but_expect_ready_is_red(self):
        payload = ready_payload()
        payload["components"][1][1] = "down"
        payload["components"][2][1] = "down"
        payload["jobs"]["counter"].update(parked=True, running=False, last_success=1)
        code, out, _, _ = self.invoke({"/readyz": reply(payload, 503)})
        self.assertEqual(code, 0, out)
        code, out, _, _ = self.invoke({"/readyz": reply(payload, 503)}, "--expect-ready")
        self.assertEqual(code, 1)
        self.assertIn("FAIL gateway-state:", out)

    def test_duplicate_keys_and_body_cap_remain_fail_closed(self):
        cases = (
            ("/health", b'{"status":"ok","status":"ok"}'),
            ("/readyz", json.dumps(ready_payload()).encode()[:-1] + b',"jobs":{}}'),
            ("/health", b'{"status":"ok"}' + b" " * probe.BODY_CAP),
            ("/readyz", json.dumps(ready_payload()).encode() + b" " * probe.BODY_CAP),
        )
        for route, raw in cases:
            with self.subTest(route=route, length=len(raw)):
                code, out, _, _ = self.invoke({route: (200, raw)})
                self.assertEqual(code, 1)
                self.assertIn("FAIL", out)

    def test_json_mode_is_discoverable_without_fetch(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), \
                mock.patch("socket.socket", side_effect=AssertionError("network forbidden")):
            try:
                probe.main(["--help"], fetch_fn=lambda _: self.fail("help fetched"))
            except SystemExit as exc:
                self.assertEqual(exc.code, 0)
        self.assertIn("--json", out.getvalue())

    def test_json_ready_round_trip_has_stable_per_check_ids(self):
        code, report, calls = self.machine(None, "--expect-ready")
        self.assertEqual(code, 0)
        self.assertEqual({check["id"] for check in report["checks"]}, CHECK_IDS)
        self.assertEqual(calls, [ORIGIN + "/health", ORIGIN + "/readyz"])
        _, again, _ = self.machine(None, "--expect-ready")
        self.assertEqual(report, again)

    def test_json_invalid_origin_has_code_remediation_and_zero_fetches(self):
        reports = []
        for secret in (TOKEN, SENTINEL):
            code, report, calls = self.machine(origin=f"ftp://user:{secret}@probe.example:bad/")
            self.assertEqual(code, 1)
            self.assertEqual(calls, [])
            failed = [check for check in report["checks"] if check["verdict"] == "FAIL"]
            self.assertTrue(failed)
            reports.append([(check["id"], check["code"], check["next_action"]) for check in failed])
        self.assertEqual(reports[0], reports[1], "origin refusal vocabulary must not depend on secrets")

    def test_json_failures_are_verdicts_not_success_evidence(self):
        nested = b"[" * 2000 + b"0" + b"]" * 2000
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_SCRATCH_DIR")) as tmp:
            evidence = Path(tmp) / "must-not-exist.json"
            code, report, _ = self.machine({"/health": (200, nested)}, "--evidence", str(evidence))
            self.assertEqual(code, 1)
            self.assertFalse(evidence.exists())
            self.assertEqual(report["verdict"], "FAIL")
            self.assertIn("liveness", {check["id"] for check in report["checks"]
                                       if check["verdict"] == "FAIL"})

    def test_json_malformed_inputs_keep_check_ids_and_no_attestation(self):
        nested = b"[" * 2000 + b"0" + b"]" * 2000
        for route, check_id in (("/health", "liveness"), ("/readyz", "readiness-shape")):
            for raw in (nested, b"null", b"{}", b'{"status":"ok","status":"ok"}',
                        b" " * (probe.BODY_CAP + 1)):
                with self.subTest(route=route, length=len(raw)):
                    code, report, _ = self.machine({route: (200, raw)})
                    self.assertEqual(code, 1)
                    self.assertEqual({check["id"] for check in report["checks"]}, CHECK_IDS)
                    row = next(check for check in report["checks"] if check["id"] == check_id)
                    self.assertEqual(row["verdict"], "FAIL")

    def test_json_bad_remote_fields_are_redacted_and_codes_stable(self):
        for field in ("build_revision", "build_id", "last_error_class", "job_name"):
            signatures = []
            for secret in (SENTINEL, TOKEN):
                payload = ready_payload()
                if field == "job_name":
                    payload["jobs"] = {secret: {"parked": False, "running": True}}
                elif field == "last_error_class":
                    payload["jobs"]["counter"].update(last_success=None, consecutive_failures=4,
                                                     last_error_class=secret)
                else:
                    payload[field] = secret
                _, report, _ = self.machine({"/readyz": reply(payload)})
                signatures.append([(check["id"], check["code"], check["next_action"])
                                   for check in report["checks"]])
            self.assertEqual(signatures[0], signatures[1], field)

    def test_json_truthful_parked_policy_matches_text_policy(self):
        payload = ready_payload()
        payload["components"][1][1] = "down"
        payload["components"][2][1] = "down"
        payload["jobs"]["counter"].update(parked=True, running=False)
        code, report, _ = self.machine({"/readyz": reply(payload, 503)})
        self.assertEqual(code, 0)
        self.assertEqual(report["verdict"], "PASS")
        code, report, _ = self.machine({"/readyz": reply(payload, 503)}, "--expect-ready")
        self.assertEqual(code, 1)
        self.assertEqual(report["verdict"], "FAIL")

    def test_json_success_keeps_separate_existing_evidence_attestation(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_SCRATCH_DIR")) as tmp:
            evidence = Path(tmp) / "success.json"
            code, report, _ = self.machine(None, "--evidence", str(evidence))
            self.assertEqual(code, 0)
            receipt = json.loads(evidence.read_text())
            self.assertEqual(receipt["result"], "pass")
            self.assertEqual(len(receipt["checks"]), 5)
            self.assertTrue(all(check.get("source") for check in receipt["checks"]))
            self.assertNotEqual(receipt, report)

    def test_text_failures_neither_create_nor_overwrite_success_evidence(self):
        bad = ready_payload()
        bad["build_revision"] = TOKEN
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_SCRATCH_DIR")) as tmp:
            evidence = Path(tmp) / "receipt.json"
            for preexisting in (False, True):
                if preexisting:
                    evidence.write_text("existing successful attestation\n")
                code, out, err, _ = self.invoke({"/readyz": reply(bad)}, "--evidence", str(evidence))
                self.assertEqual(code, 1)
                self.assert_redacted(out + err)
                if preexisting:
                    self.assertEqual(evidence.read_text(), "existing successful attestation\n")
                else:
                    self.assertFalse(evidence.exists())

    def test_real_transport_still_uses_get_and_refuses_redirects_offline(self):
        seen = []
        reads = []
        case = self

        class Response(io.BytesIO):
            status = 200

            def read(self, size=-1):
                reads.append(size)
                return super().read(size)

        def open_double(opener, request, *args, **kwargs):
            seen.append((request.get_method(), request.full_url, request.data))
            redirect_handlers = [handler for handler in opener.handlers
                                 if isinstance(handler, urllib.request.HTTPRedirectHandler)]
            case.assertTrue(redirect_handlers)
            for handler in redirect_handlers:
                redirected = handler.redirect_request(request, None, 302, "Found", {},
                                                       "https://redirect.example/" + TOKEN)
                case.assertIsNone(redirected, "redirect must never issue a second request")
            return Response(b'{"status":"ok"}')

        with mock.patch.object(urllib.request.OpenerDirector, "open", open_double), \
                mock.patch("socket.socket", side_effect=AssertionError("network forbidden")):
            status, raw = probe.fetch(ORIGIN + "/health", 5)
        self.assertEqual((status, raw), (200, b'{"status":"ok"}'))
        self.assertEqual(seen, [("GET", ORIGIN + "/health", None)])
        self.assertEqual(reads, [probe.BODY_CAP + 1])


if __name__ == "__main__":
    unittest.main()

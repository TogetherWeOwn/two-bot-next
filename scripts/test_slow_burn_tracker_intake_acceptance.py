"""Protected offline intake contract; only synthetic Python subprocesses, no services."""

import contextlib
import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
tracker = importlib.import_module("slow_burn_tracker")

REAL_RUN = tracker.subprocess.run
INVALID_UTF8 = b'\xffremote-body-sentinel'
NOW = 1_700_000_000
# An acceptance ceiling, not a required implementation constant: smaller finite
# caps are fine. Normal receipts, 84-snapshot rings and 50 issue rows fit easily.
INTAKE_CEILING = 1024 * 1024
TITLE = "[voice-join-slow-burn] fixture"
VERDICT = {"slow": True, "fast": False, "held": False,
           "slowCovered": True, "ring": []}
ENV = {"GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
       "GITHUB_SERVER_URL": "https://github.com", "GITHUB_RUN_ID": "1",
       "GH_TOKEN": "synthetic-fixture-only"}


def nested_json():
    return "[" * 10000 + '"remote-body-sentinel"' + "]" * 10000


def oversized_json(value):
    # Valid JSON below the character ceiling but above the UTF-8 byte ceiling.
    value = dict(value, padding="é" * (INTAKE_CEILING // 2 + 1))
    return json.dumps(value, ensure_ascii=False)


class SizedReadOnly:
    """A real file proxy that refuses an unbounded/over-budget cache read."""

    def __init__(self, stream):
        self.stream = stream

    def __enter__(self):
        self.stream.__enter__()
        return self

    def __exit__(self, *args):
        return self.stream.__exit__(*args)

    def __getattr__(self, name):
        return getattr(self.stream, name)

    def read(self, size=-1):
        if size < 0 or size > INTAKE_CEILING + 1:
            raise AssertionError("cache read must be sized before buffering")
        return self.stream.read(size)


class TrackerIntakeAcceptanceTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        previous = Path.cwd()
        os.chdir(self.dir)
        self.addCleanup(os.chdir, previous)
        # Fail closed if a fixture accidentally reaches a live boundary.
        self.no_network = mock.patch.object(
            tracker.urllib.request, "build_opener",
            side_effect=AssertionError("offline acceptance must not scrape"))
        self.no_network.start()
        self.addCleanup(self.no_network.stop)
        self.no_process = mock.patch.object(
            tracker.subprocess, "run",
            side_effect=AssertionError("offline acceptance must not execute gh/node"))
        self.no_process.start()
        self.addCleanup(self.no_process.stop)

    def expect_track_error(self, call, prefix):
        try:
            call()
        except tracker.TrackError as error:
            message = str(error)
            self.assertTrue(message.startswith(prefix), message)
            self.assertLess(len(message), 160)
            self.assertNotIn("remote-body-sentinel", message)
            self.assertNotIn("RecursionError", message)
            return
        except Exception as error:
            self.fail(f"expected fixed-vocabulary TrackError, got {type(error).__name__}")
        self.fail("malformed or over-cap intake must raise TrackError")

    def driver_answer(self, raw):
        completed = mock.Mock(returncode=0, stdout=raw, stderr="")
        with mock.patch.object(tracker.subprocess, "run", return_value=completed):
            return tracker.run_driver("node", "driver.mjs", "expo.prom", "ring.json", NOW)

    def issue_answer(self, raw):
        with mock.patch.object(tracker, "gh", return_value=raw):
            return tracker.open_tracking_issues("TogetherWeOwn/two-bot-next", "fixture", "slow-burn", "[voice-join-slow-burn]")

    def main_with_answer(self, boundary, raw, slow=True):
        output = io.StringIO()
        evidence = self.dir / "receipt.json"
        evidence.unlink(missing_ok=True)
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(output))
            stack.enter_context(contextlib.redirect_stderr(output))
            if boundary == "driver":
                completed = mock.Mock(returncode=0, stdout=raw, stderr="")
                stack.enter_context(mock.patch.object(tracker.subprocess, "run", return_value=completed))
                stack.enter_context(mock.patch.object(tracker, "reconcile", side_effect=AssertionError("bad driver output must never reconcile")))
            else:
                stack.enter_context(mock.patch.object(tracker, "run_driver", return_value=dict(VERDICT, slow=slow)))
                def list_only(args, *unused):
                    self.assertEqual(args[0], "list", "bad issue rows must never produce a write")
                    return raw
                stack.enter_context(mock.patch.object(tracker, "gh", side_effect=list_only))
            try:
                code = tracker.main([
                    "--fixture", "fire" if slow else "clear", "--evidence-out", str(evidence),
                    "--exposition-out", str(self.dir / "expo.prom"),
                    "--ring", str(self.dir / "ring.json"),
                    "--coverage-path", str(self.dir / "coverage.json")], ENV)
            except Exception as error:
                self.fail(f"main must write a receipt and exit 1, not escape {type(error).__name__}")
        self.assertEqual(code, 1)
        self.assertTrue(evidence.is_file(), "a refused input still needs an evidence receipt")
        receipt = json.loads(evidence.read_text(encoding="utf-8"))
        self.assertEqual(receipt["action"], "none")
        self.assertIsNone(receipt["issue"])
        if boundary == "driver":
            self.assertEqual(receipt["verdict"], "unknown")
        self.assertIn("::error::", output.getvalue())
        self.assertNotIn("Traceback", output.getvalue())
        self.assertNotIn("remote-body-sentinel", output.getvalue())
        return output.getvalue()

    def reconcile_rows(self, rows, slow=True):
        calls = []
        def fake_gh(args, *unused):
            if args[0] == "list":
                return json.dumps(rows)
            body = (Path(args[args.index("--body-file") + 1]).read_text()
                    if "--body-file" in args else None)
            calls.append((args[0], args[1], body))
            return ""
        with mock.patch.object(tracker, "gh", side_effect=fake_gh):
            action = tracker.reconcile(
                repo=ENV["GITHUB_REPOSITORY"], token="fixture", label="slow-burn",
                prefix="[voice-join-slow-burn]", origin="fixture:fire",
                run_url="https://example.com/run", now=NOW, slow=slow,
                fast_also=False, dry_run=False)
        return action, calls

    def refuse_rows_before_writes(self, rows, slow):
        def list_only(args, *unused):
            self.assertEqual(args[0], "list", "invalid rows must never produce a write")
            return json.dumps(rows)
        with mock.patch.object(tracker, "gh", side_effect=list_only):
            self.expect_track_error(lambda: tracker.reconcile(
                repo=ENV["GITHUB_REPOSITORY"], token="fixture", label="slow-burn",
                prefix="[voice-join-slow-burn]", origin="fixture:fire",
                run_url="https://example.com/run", now=NOW, slow=slow,
                fast_also=False, dry_run=False),
                "issue reconcile failed: open issues are unreadable")

    def invoke_main(self, extra=(), slow=True):
        evidence = self.dir / "receipt.json"
        evidence.unlink(missing_ok=True)
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            with mock.patch.object(tracker.time, "time", return_value=NOW):
                try:
                    code = tracker.main([
                        "--fixture", "fire" if slow else "clear",
                        "--evidence-out", str(evidence),
                        "--exposition-out", str(self.dir / "expo.prom"),
                        "--ring", str(self.dir / "ring.json"),
                        "--coverage-path", str(self.dir / "coverage.json"),
                        *extra], ENV)
                except Exception as error:
                    self.fail(f"main must return with a receipt, not escape {type(error).__name__}")
        self.assertTrue(evidence.is_file(), "every attempt needs a fresh receipt")
        receipt = json.loads(evidence.read_text(encoding="utf-8"))
        self.assertNotIn("Traceback", output.getvalue())
        self.assertNotIn("remote-body-sentinel", output.getvalue())
        self.assertNotIn("remote-body-sentinel", evidence.read_text())
        return code, receipt, output.getvalue()

    def assert_failure_receipt(self, result, verdict, message):
        code, receipt, output = result
        self.assertEqual(code, 1)
        self.assertEqual((receipt["action"], receipt["issue"], receipt["verdict"]),
                         ("none", None, verdict))
        self.assertEqual(output.strip(), "::error::" + message)

    def test_missing_body_is_refused_before_fire_or_clear_writes(self):
        for slow in (True, False):
            with self.subTest(slow=slow):
                self.refuse_rows_before_writes([
                    {"number": 9, "title": TITLE, "body": "valid"},
                    {"number": 7, "title": TITLE}], slow)

    def test_missing_body_fire_and_clear_keep_failure_receipts(self):
        for slow in (True, False):
            with self.subTest(slow=slow):
                output = self.main_with_answer("gh", json.dumps([
                    {"number": 7, "title": TITLE}]), slow=slow)
                self.assertEqual(output.strip(),
                                 "::error::issue reconcile failed: open issues are unreadable")

    def test_explicit_null_and_string_bodies_remain_valid(self):
        for body in (None, "Original issue text.\n\nKeep this paragraph."):
            for slow in (True, False):
                with self.subTest(body=body, slow=slow):
                    rows = [{"number": 7, "title": TITLE, "body": body}]
                    self.assertEqual(self.issue_answer(json.dumps(rows)), rows)
                    action, calls = self.reconcile_rows(rows, slow)
                    self.assertEqual(action, ("refreshed" if slow else "closed", 7))
                    self.assertEqual([call[:2] for call in calls],
                                     [("comment", "7"), ("edit" if slow else "close", "7")])
                    if slow and body is not None:
                        self.assertTrue(calls[1][2].startswith(body + "\n\n"))

    def test_malformed_and_oversized_marker_timestamps_are_absent(self):
        for stamp in ("9" * 5000, "not-a-time", "-1", ""):
            with self.subTest(length=len(stamp)):
                body = "<!-- slow-burn-tracker:last-update=" + stamp + " -->"
                try:
                    tick = tracker.marker_time(body)
                except Exception as error:
                    self.fail(f"untrusted marker must be absent, not raise {type(error).__name__}")
                self.assertEqual(tick, 0)

    def test_oversized_marker_refresh_preserves_text_and_writes_receipt(self):
        first = "Original incident narrative.\n\nKeep the operator notes.\n\n"
        last = "\n\nKeep the trailing follow-up.\n"
        body = first + "<!-- slow-burn-tracker:last-update=" + "9" * 5000 + " -->" + last
        raw = json.dumps([{"number": 7, "title": TITLE, "body": body}])
        self.assertLess(len(raw.encode("utf-8")), INTAKE_CEILING)
        writes = []
        def fake_gh(args, *unused):
            if args[0] == "list":
                return raw
            self.assertIn(args[0], ("comment", "edit"))
            self.assertEqual(args[1], "7")
            text = Path(args[args.index("--body-file") + 1]).read_text()
            writes.append((args[0], text))
            return ""
        with mock.patch.object(tracker, "gh", side_effect=fake_gh):
            with mock.patch.object(tracker, "run_driver", return_value=VERDICT):
                code, receipt, output = self.invoke_main()
        self.assertEqual(code, 0)
        self.assertEqual((receipt["action"], receipt["issue"], receipt["verdict"]),
                         ("refreshed", 7, "slow-fire"))
        self.assertNotIn("::error::", output)
        self.assertEqual([verb for verb, text in writes], ["comment", "edit"])
        self.assertTrue(writes[1][1].startswith(first))
        self.assertIn(last.strip(), writes[1][1])
        self.assertIn("<!-- slow-burn-tracker:last-update=1700000000 -->", writes[1][1])
        self.assertEqual(tracker.marker_time(writes[1][1]), NOW)

    def test_healthy_marker_round_trip_and_refresh_boundary(self):
        body = tracker.with_marker("Keep this narrative.", NOW)
        self.assertEqual(tracker.marker_time(body), NOW)
        for age, expected in ((21599, "already-open"), (21600, "refreshed")):
            with self.subTest(age=age):
                marked = tracker.with_marker("Keep this narrative.", NOW - age)
                action, calls = self.reconcile_rows([
                    {"number": 7, "title": TITLE, "body": marked}])
                self.assertEqual(action, (expected, 7))
                self.assertEqual([call[0] for call in calls],
                                 [] if expected == "already-open" else ["comment", "edit"])

    def invalid_driver_answer(self, argv, **kwargs):
        self.assertNotEqual(argv[0], "gh", "invalid driver must never reconcile")
        raw = INVALID_UTF8
        # Model subprocess.run's actual text-mode decode, not an always-string
        # Mock: restoring text=True must expose a missing decode guard.
        if kwargs.get("text") or kwargs.get("universal_newlines") or kwargs.get("encoding"):
            raw = raw.decode(kwargs.get("encoding") or "utf-8",
                             errors=kwargs.get("errors") or "strict")
        return tracker.subprocess.CompletedProcess(argv, 0, stdout=raw, stderr=b"")

    def test_driver_decode_guard_honors_subprocess_text_mode(self):
        with mock.patch.object(tracker.subprocess, "run", side_effect=self.invalid_driver_answer):
            self.expect_track_error(lambda: tracker.run_driver(
                "synthetic-python", "fixture.py", "expo.prom", "ring.json", NOW),
                "burn evaluation failed:")
            self.assert_failure_receipt(self.invoke_main(), "unknown",
                                       "burn evaluation failed: driver answer is not bounded JSON")

    def test_valid_utf8_driver_bytes_still_yield_verdict(self):
        self.assertEqual(self.driver_answer(json.dumps(VERDICT).encode("utf-8")), VERDICT)

    def test_invalid_utf8_from_real_synthetic_driver_is_safe(self):
        driver = self.dir / "invalid_driver.py"
        driver.write_text("import sys\nsys.stdout.buffer.write(b'\\xffremote-body-sentinel')\n")
        def only_synthetic_driver(argv, **kwargs):
            self.assertEqual(argv[:2], [sys.executable, str(driver)])
            return REAL_RUN(argv, env={"PYTHONIOENCODING": "utf-8"}, **kwargs)
        with mock.patch.object(tracker.subprocess, "run", side_effect=only_synthetic_driver):
            self.expect_track_error(lambda: tracker.run_driver(
                sys.executable, driver, "expo.prom", "ring.json", NOW),
                "burn evaluation failed:")
            self.assert_failure_receipt(self.invoke_main([
                "--node", sys.executable, "--driver", str(driver)]), "unknown",
                "burn evaluation failed: driver answer is not bounded JSON")

    def test_real_gh_wrapper_contains_subprocess_decode_errors(self):
        def invalid_gh(argv, **kwargs):
            self.assertEqual(argv[:3], ["gh", "issue", "list"])
            raise UnicodeDecodeError("utf-8", INVALID_UTF8, 0, 1, "remote-body-sentinel")
        with mock.patch.object(tracker.subprocess, "run", side_effect=invalid_gh):
            self.expect_track_error(lambda: tracker.gh(
                ["list"], ENV["GITHUB_REPOSITORY"], "fixture"),
                "issue reconcile failed: gh answer is not text")
            for slow in (True, False):
                with self.subTest(slow=slow):
                    with mock.patch.object(tracker, "run_driver", return_value=dict(VERDICT, slow=slow)):
                        self.assert_failure_receipt(self.invoke_main(slow=slow),
                            "slow-fire" if slow else "clear",
                            "issue reconcile failed: open issues are unreadable")

    def test_nonpositive_issue_numbers_are_refused_before_fire_or_clear_writes(self):
        for number in (0, -1):
            for slow in (True, False):
                with self.subTest(number=number, slow=slow):
                    self.refuse_rows_before_writes([
                        {"number": 9, "title": TITLE, "body": "valid"},
                        {"number": number, "title": TITLE, "body": "invalid"}], slow)

    def test_nonpositive_issue_numbers_keep_fire_and_clear_failure_receipts(self):
        for number in (0, -1):
            for slow in (True, False):
                with self.subTest(number=number, slow=slow):
                    self.main_with_answer("gh", json.dumps([
                        {"number": number, "title": TITLE, "body": "invalid"}]), slow=slow)

    def test_positive_issue_numbers_select_oldest_for_fire_and_clear(self):
        for slow in (True, False):
            with self.subTest(slow=slow):
                action, calls = self.reconcile_rows([
                    {"number": 9, "title": TITLE, "body": "nine"},
                    {"number": 1, "title": TITLE, "body": "one"},
                    {"number": 7, "title": TITLE, "body": "seven"}], slow)
                self.assertEqual(action, ("refreshed" if slow else "closed", 1))
                self.assertEqual([call[:2] for call in calls],
                                 [("comment", "1"), ("edit" if slow else "close", "1")])

    def test_deep_fixture_reaches_the_json_recursion_guard(self):
        with self.assertRaises(RecursionError):
            json.loads(nested_json())

    def test_deep_driver_json_is_a_track_error(self):
        self.expect_track_error(lambda: self.driver_answer(nested_json()), "burn evaluation failed:")

    def test_deep_gh_json_is_a_track_error(self):
        self.expect_track_error(lambda: self.issue_answer(nested_json()), "issue reconcile failed:")

    def test_deep_driver_json_still_writes_failure_receipt(self):
        self.main_with_answer("driver", nested_json())

    def test_deep_gh_json_still_writes_failure_receipt(self):
        self.main_with_answer("gh", nested_json())

    def test_oversized_driver_json_is_refused_before_json_decode(self):
        raw = oversized_json(VERDICT)
        self.assertLess(len(raw), INTAKE_CEILING)
        self.assertGreater(len(raw.encode("utf-8")), INTAKE_CEILING)
        with mock.patch.object(tracker.json, "loads", side_effect=AssertionError("over-cap driver JSON must not be decoded")):
            self.expect_track_error(lambda: self.driver_answer(raw), "burn evaluation failed:")

    def test_oversized_gh_json_is_refused_before_json_decode(self):
        raw = "[" + oversized_json({"number": 7, "title": TITLE, "body": ""}) + "]"
        self.assertLess(len(raw), INTAKE_CEILING)
        self.assertGreater(len(raw.encode("utf-8")), INTAKE_CEILING)
        with mock.patch.object(tracker.json, "loads", side_effect=AssertionError("over-cap gh JSON must not be decoded")):
            self.expect_track_error(lambda: self.issue_answer(raw), "issue reconcile failed:")

    def test_oversized_driver_answer_writes_failure_receipt(self):
        self.main_with_answer("driver", oversized_json(VERDICT))

    def test_oversized_gh_answer_writes_failure_receipt(self):
        self.main_with_answer("gh", "[" + oversized_json({"number": 7, "title": TITLE, "body": ""}) + "]")

    def test_issue_numbers_are_validated_before_sort_or_use(self):
        valid = {"number": 7, "title": TITLE, "body": ""}
        for invalid in ("8", None, True, 8.5, [], {}):
            for slow in (True, False):
                with self.subTest(number=invalid, slow=slow):
                    raw = json.dumps([valid, {"number": invalid, "title": TITLE, "body": ""}])
                    with mock.patch.object(tracker, "gh", return_value=raw):
                        self.expect_track_error(lambda: tracker.reconcile(
                            repo="TogetherWeOwn/two-bot-next", token="fixture",
                            label="slow-burn", prefix="[voice-join-slow-burn]",
                            origin="fixture:fire", run_url="https://example.com/run",
                            now=NOW, slow=slow, fast_also=False, dry_run=True),
                            "issue reconcile failed:")

    def test_missing_issue_number_is_not_an_implicit_zero(self):
        raw = json.dumps([{"title": TITLE, "body": ""}])
        self.expect_track_error(lambda: self.issue_answer(raw), "issue reconcile failed:")

    def test_string_issue_number_still_writes_failure_receipt(self):
        self.main_with_answer("gh", json.dumps([
            {"number": 7, "title": TITLE, "body": ""},
            {"number": "8", "title": TITLE, "body": ""}]))

    def test_small_valid_driver_and_issue_json_survive(self):
        self.assertEqual(self.driver_answer(json.dumps(VERDICT)), VERDICT)
        rows = [{"number": 7, "title": TITLE, "body": "fixture"}]
        self.assertEqual(self.issue_answer(json.dumps(rows)), rows)

    def test_valid_integer_issue_numbers_select_oldest(self):
        raw = json.dumps([{"number": 9, "title": TITLE, "body": ""},
                          {"number": 7, "title": TITLE, "body": ""}])
        with mock.patch.object(tracker, "gh", return_value=raw):
            with contextlib.redirect_stdout(io.StringIO()):
                action = tracker.reconcile(
                    repo="TogetherWeOwn/two-bot-next", token="fixture",
                    label="slow-burn", prefix="[voice-join-slow-burn]",
                    origin="fixture:fire", run_url="https://example.com/run",
                    now=NOW, slow=False, fast_also=False, dry_run=True)
        self.assertEqual(action, ("would-close", 7))

    def test_held_verdict_never_files_refreshes_or_closes(self):
        ring = self.dir / "ring.json"
        coverage = self.dir / "coverage.json"
        ring.write_text('[{"t":1699999700,"received":25,"duplicate":0,"fastMoves":24}]')
        coverage.write_text('{"last_covered":1699996400}')
        before = (ring.read_bytes(), coverage.read_bytes())
        held = dict(VERDICT, held=True, slow=False, slowCovered=False)
        env = dict(ENV, STAGING_WORKER_URL="https://staging.example", METRICS_SCRAPE_TOKEN="fixture")
        with mock.patch.object(tracker, "scrape_metrics", return_value="fixture corrupt exposition"):
            with mock.patch.object(tracker, "run_driver", return_value=held):
                with mock.patch.object(tracker, "reconcile", side_effect=AssertionError("held must never reconcile")):
                    with contextlib.redirect_stdout(io.StringIO()):
                        code = tracker.main([
                            "--ring", str(ring), "--coverage-path", str(coverage),
                            "--exposition-out", str(self.dir / "expo.prom"),
                            "--evidence-out", str(self.dir / "receipt.json")], env)
        self.assertEqual(code, 0)
        receipt = json.loads((self.dir / "receipt.json").read_text())
        self.assertEqual((receipt["verdict"], receipt["action"], receipt["issue"]), ("held", "none", None))
        self.assertEqual((ring.read_bytes(), coverage.read_bytes()), before)

    def test_oversized_ring_rebuilds_without_decoding(self):
        path = self.dir / "ring.json"
        path.write_text("[" + oversized_json({"t": NOW, "received":25, "duplicate":0, "fastMoves":24}) + "]", encoding="utf-8")
        with mock.patch.object(tracker.json, "loads", side_effect=AssertionError("over-cap ring must not be decoded")):
            self.assertEqual(tracker.load_ring(str(path)), [])

    def test_oversized_sidecar_seeds_fresh_tracking(self):
        path = self.dir / "coverage.json"
        path.write_text(oversized_json({"last_covered": NOW - 3600, "tracking_since": NOW - 7200}), encoding="utf-8")
        self.assertIsNone(tracker.load_covered_at(str(path)))
        self.assertIsNone(tracker.load_tracking_since(str(path)))
        tracker.seed_tracking_since(str(path), NOW)
        self.assertEqual(json.loads(path.read_text()), {"tracking_since": NOW})

    def test_over_cap_cache_reads_are_sized_before_buffering(self):
        real_open = io.open
        for kind in ("ring", "sidecar"):
            with self.subTest(kind=kind):
                path = self.dir / f"{kind}.json"
                path.write_text(" " * (INTAKE_CEILING + 1) + ("[]" if kind == "ring" else "{}"))
                def guarded_open(file, *args, **kwargs):
                    stream = real_open(file, *args, **kwargs)
                    return SizedReadOnly(stream) if Path(file) == path else stream
                with mock.patch.object(io, "open", side_effect=guarded_open):
                    if kind == "ring":
                        self.assertEqual(tracker.load_ring(str(path)), [])
                    else:
                        self.assertIsNone(tracker.load_covered_at(str(path)))

    def test_small_cache_files_remain_readable(self):
        path = self.dir / "ring.json"
        rows = [{"t": NOW, "received":25, "duplicate":0, "fastMoves":24}]
        path.write_text(json.dumps(rows))
        self.assertEqual(tracker.load_ring(str(path)), rows)
        sidecar = self.dir / "coverage.json"
        sidecar.write_text(json.dumps({"last_covered":NOW - 60, "tracking_since":NOW - 3600}))
        self.assertEqual(tracker.load_covered_at(str(sidecar)), NOW - 60)
        self.assertEqual(tracker.load_tracking_since(str(sidecar)), NOW - 3600)


if __name__ == "__main__":
    unittest.main()

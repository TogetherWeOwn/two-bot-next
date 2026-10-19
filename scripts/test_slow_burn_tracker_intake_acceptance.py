"""Protected offline acceptance contract for tracker intake; no live subprocesses."""

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
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
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

    def main_with_answer(self, boundary, raw):
        output = io.StringIO()
        evidence = self.dir / "receipt.json"
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(output))
            stack.enter_context(contextlib.redirect_stderr(output))
            if boundary == "driver":
                completed = mock.Mock(returncode=0, stdout=raw, stderr="")
                stack.enter_context(mock.patch.object(tracker.subprocess, "run", return_value=completed))
                stack.enter_context(mock.patch.object(tracker, "reconcile", side_effect=AssertionError("bad driver output must never reconcile")))
            else:
                stack.enter_context(mock.patch.object(tracker, "run_driver", return_value=VERDICT))
                def list_only(args, *unused):
                    self.assertEqual(args[0], "list", "bad issue rows must never produce a write")
                    return raw
                stack.enter_context(mock.patch.object(tracker, "gh", side_effect=list_only))
            try:
                code = tracker.main([
                    "--fixture", "fire", "--evidence-out", str(evidence),
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

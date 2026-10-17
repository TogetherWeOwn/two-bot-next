"""Offline slow-burn tracker fixtures; stdlib only, no network access."""

import importlib
import json
import shutil
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
tracker = importlib.import_module("slow_burn_tracker")

REPO = "TogetherWeOwn/two-bot-next"
RUN_URL = "https://github.com/TogetherWeOwn/two-bot-next/actions/runs/1"
NODE = shutil.which("node")
DRIVER = Path(__file__).resolve().parent.parent / "wrangler" / "scripts" / "slow-burn-decide.mjs"


def base_env(extra=None):
    env = {"GITHUB_REPOSITORY": REPO, "GITHUB_RUN_ID": "1",
           "GITHUB_SERVER_URL": "https://github.com", "GH_TOKEN": "test-token"}
    env.update(extra or {})
    return env


class FakeGh:
    """In-memory `gh issue` double driven by argv inspection."""

    def __init__(self, issues=None):
        self.issues = issues if issues is not None else []
        self.calls = []
        self.next_number = max([issue["number"] for issue in self.issues] + [0]) + 1

    def __call__(self, argv, **kwargs):
        self.calls.append(argv)
        verb = argv[2]
        if verb == "list":
            return mock.Mock(returncode=0, stdout=json.dumps(self.issues), stderr="")
        if verb == "create":
            number = self.next_number
            self.next_number += 1
            title = argv[argv.index("--title") + 1]
            self.issues.append({"number": number, "title": title, "body": ""})
            return mock.Mock(
                returncode=0,
                stdout=f"https://github.com/{REPO}/issues/{number}\n", stderr="")
        if verb == "comment":
            return mock.Mock(returncode=0, stdout="", stderr="")
        if verb == "edit":
            return mock.Mock(returncode=0, stdout="", stderr="")
        if verb == "close":
            number = int(argv[3])
            self.issues = [issue for issue in self.issues if issue["number"] != number]
            return mock.Mock(returncode=0, stdout="", stderr="")
        raise AssertionError(f"unexpected gh call: {argv}")


class TrackerUnitTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def test_marker_round_trip(self):
        now = 1_700_000_000
        body = tracker.with_marker("", now)
        self.assertEqual(tracker.marker_time(body), now)
        self.assertEqual(tracker.marker_time("no marker here"), 0)
        refreshed = tracker.with_marker(body, now + 10)
        self.assertEqual(tracker.marker_time(refreshed), now + 10)
        self.assertEqual(refreshed.count("slow-burn-tracker"), 1)

    def test_title_prefix_is_dedup_key(self):
        title = tracker.issue_title(tracker.TITLE_PREFIX)
        self.assertTrue(title.startswith(tracker.TITLE_PREFIX))
        self.assertNotIn("TOG-", title)

    def test_body_is_public_safe(self):
        body = tracker.issue_body("https://staging.example", RUN_URL, 1_700_000_000, False)
        for secret in ("METRICS_SCRAPE_TOKEN", "Bearer", "GH_TOKEN", "TOG-"):
            self.assertNotIn(secret, body)
        self.assertIn(tracker.RUNBOOK_URL, body)

    def test_unconfigured_exits_zero_with_warning(self):
        evidence = str(self.dir / "evidence.json")
        with mock.patch("sys.stdout") as stdout:
            code = tracker.main(
                ["--evidence-out", evidence, "--exposition-out",
                 str(self.dir / "expo.prom")],
                base_env({"STAGING_WORKER_URL": "", "METRICS_SCRAPE_TOKEN": ""}))
        self.assertEqual(code, 0)
        self.assertIn("warning", stdout.write.call_args_list[0].args[0])
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "unconfigured")

    def test_scrape_failure_never_reconciles(self):
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        dead = mock.Mock()
        dead.open.side_effect = tracker.urllib.error.URLError("down")
        with mock.patch.object(tracker.urllib.request, "build_opener",
                               return_value=dead):
            with mock.patch("subprocess.run",
                             side_effect=AssertionError("gh must not run")):
                code = tracker.main(
                    ["--evidence-out", evidence, "--exposition-out",
                     str(self.dir / "expo.prom")],
                    env)
        self.assertEqual(code, 1)
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["action"], "none")

    def test_fire_creates_one_issue(self):
        fake = FakeGh()
        with mock.patch("subprocess.run", fake):
            action, number = tracker.reconcile(
                repo=REPO, token="t", label="slow-burn",
                prefix=tracker.TITLE_PREFIX, origin="fixture:fire",
                run_url=RUN_URL, now=1_700_000_000,
                slow=True, fast_also=False, dry_run=False)
        self.assertEqual(action, "created")
        self.assertEqual(number, 1)
        creates = [call for call in fake.calls if call[2] == "create"]
        self.assertEqual(len(creates), 1)

    def test_refire_while_open_neither_duplicates_nor_spams(self):
        now = 1_700_000_000
        body = tracker.with_marker("firing", now - 3600)
        fake = FakeGh([{"number": 7,
                        "title": tracker.issue_title(tracker.TITLE_PREFIX),
                        "body": body}])
        with mock.patch("subprocess.run", fake):
            action, number = tracker.reconcile(
                repo=REPO, token="t", label="slow-burn",
                prefix=tracker.TITLE_PREFIX, origin="fixture:fire",
                run_url=RUN_URL, now=now,
                slow=True, fast_also=False, dry_run=False)
        self.assertEqual((action, number), ("already-open", 7))
        writes = [call for call in fake.calls if call[2] in ("create", "comment")]
        self.assertEqual(writes, [])

    def test_stale_open_issue_gets_one_refresh(self):
        now = 1_700_000_000
        body = tracker.with_marker("firing", now - 7 * 3600)
        fake = FakeGh([{"number": 7,
                        "title": tracker.issue_title(tracker.TITLE_PREFIX),
                        "body": body}])
        with mock.patch("subprocess.run", fake):
            action, number = tracker.reconcile(
                repo=REPO, token="t", label="slow-burn",
                prefix=tracker.TITLE_PREFIX, origin="fixture:fire",
                run_url=RUN_URL, now=now,
                slow=True, fast_also=False, dry_run=False)
        self.assertEqual((action, number), ("refreshed", 7))
        verbs = sorted(call[2] for call in fake.calls if call[2] != "list")
        self.assertEqual(verbs, ["comment", "edit"])

    def test_clear_closes_with_a_resolve_comment(self):
        fake = FakeGh([{"number": 7,
                        "title": tracker.issue_title(tracker.TITLE_PREFIX),
                        "body": tracker.with_marker("firing", 1_700_000_000)}])
        with mock.patch("subprocess.run", fake):
            action, number = tracker.reconcile(
                repo=REPO, token="t", label="slow-burn",
                prefix=tracker.TITLE_PREFIX, origin="https://staging.example",
                run_url=RUN_URL, now=1_700_000_000 + 8 * 3600,
                slow=False, fast_also=False, dry_run=False)
        self.assertEqual((action, number), ("closed", 7))
        verbs = sorted(call[2] for call in fake.calls if call[2] != "list")
        self.assertEqual(verbs, ["close", "comment"])
        self.assertEqual(fake.issues, [])

    def test_clear_with_none_open_is_a_noop(self):
        fake = FakeGh()
        with mock.patch("subprocess.run", fake):
            action, number = tracker.reconcile(
                repo=REPO, token="t", label="slow-burn",
                prefix=tracker.TITLE_PREFIX, origin="https://staging.example",
                run_url=RUN_URL, now=1_700_000_000,
                slow=False, fast_also=False, dry_run=False)
        self.assertEqual((action, number), ("none-open", None))
        writes = [call for call in fake.calls if call[2] != "list"]
        self.assertEqual(writes, [])

    def test_held_verdict_decides_nothing(self):
        evidence = str(self.dir / "evidence.json")
        held = {"slow": False, "fast": False, "held": True, "ring": [],
                "slowCovered": False}
        with mock.patch.object(tracker, "run_driver", return_value=held):
            with mock.patch("subprocess.run",
                             side_effect=AssertionError("gh must not run")):
                code = tracker.main(
                    ["--fixture", "fire", "--evidence-out", evidence,
                     "--exposition-out", str(self.dir / "expo.prom")],
                    base_env())
        self.assertEqual(code, 0)
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "held")
        self.assertEqual(stored["action"], "none")

    def test_main_without_argv_reads_process_arguments(self):
        # A bare main() must read sys.argv: with `argv or []` a fixture
        # dispatch silently ran live and wrote under the real label.
        evidence = str(self.dir / "evidence.json")
        argv = ["slow_burn_tracker.py", "--fixture", "fire", "--dry-run",
                "--evidence-out", evidence,
                "--exposition-out", str(self.dir / "expo.prom"),
                "--ring", str(self.dir / "ring.json"),
                "--label", "slow-burn-test",
                "--title-prefix", "[slow-burn-test]",
                "--node", "node", "--driver", "driver.mjs"]
        held = {"slow": True, "fast": False, "held": True, "ring": [],
                "slowCovered": False}
        with mock.patch.object(tracker.sys, "argv", argv):
            with mock.patch.object(tracker, "run_driver", return_value=held):
                with mock.patch("subprocess.run",
                                 side_effect=AssertionError("gh must not run")):
                    code = tracker.main(None, base_env())
        self.assertEqual(code, 0)
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "held")
        self.assertEqual(stored["action"], "none")

    def test_uncovered_windows_never_close_and_keep_the_ring(self):
        # Slow windows without full history are unknown, not clear: the open
        # card stays open, yet the ring still persists so history rebuilds.
        now = 1_700_000_000
        body = tracker.with_marker("firing", now - 3600)
        fake = FakeGh([{"number": 7,
                        "title": tracker.issue_title(tracker.TITLE_PREFIX),
                        "body": body}])
        next_ring = [{"t": now, "received": 10, "duplicate": 0, "fastMoves": 9}]
        uncovered = {"slow": False, "fast": False, "held": False,
                     "ring": next_ring, "slowCovered": False}
        ring_path = str(self.dir / "ring.json")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=uncovered):
                with mock.patch("subprocess.run", fake):
                    code = tracker.main(
                        ["--ring", ring_path, "--evidence-out", evidence,
                         "--exposition-out", str(self.dir / "expo.prom")],
                        env)
        self.assertEqual(code, 0)
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["action"], "none")
        self.assertIsNone(stored["issue"])
        self.assertEqual(fake.calls, [])
        self.assertEqual(len(fake.issues), 1)
        self.assertEqual(json.loads(Path(ring_path).read_text()), next_ring)

    def test_dry_run_writes_nothing(self):
        fake = FakeGh()
        with mock.patch("subprocess.run", fake):
            action, _ = tracker.reconcile(
                repo=REPO, token="t", label="slow-burn-test",
                prefix=tracker.TITLE_PREFIX, origin="fixture:fire",
                run_url=RUN_URL, now=1_700_000_000,
                slow=True, fast_also=False, dry_run=True)
        self.assertEqual(action, "would-create")
        writes = [call for call in fake.calls if call[2] != "list"]
        self.assertEqual(writes, [])


@unittest.skipUnless(NODE and DRIVER.exists(), "node driver integration needs node")
class TrackerDriverTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def run_fixture(self, kind):
        now = int(time.time())
        exposition, ring = tracker.fixture_inputs(kind, now)
        expo = self.dir / "expo.prom"
        expo.write_text(exposition, encoding="utf-8")
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(ring), encoding="utf-8")
        return tracker.run_driver(NODE, str(DRIVER), expo, ring_path, now)

    def test_fire_fixture_trips_only_the_slow_ticket(self):
        verdict = self.run_fixture("fire")
        self.assertTrue(verdict["slow"])
        self.assertFalse(verdict.get("fast"))
        self.assertTrue(verdict.get("slowCovered"))
        self.assertGreater(len(verdict["ring"]), 0)

    def test_clear_fixture_stays_silent(self):
        verdict = self.run_fixture("clear")
        self.assertFalse(verdict["slow"])
        self.assertFalse(verdict.get("fast"))
        self.assertTrue(verdict.get("slowCovered"))

    def test_missing_join_series_holds_instead_of_clearing(self):
        now = int(time.time())
        _, ring = tracker.fixture_inputs("fire", now)
        expo = self.dir / "expo.prom"
        expo.write_text('two_bot_voice_operations_total{op="create",outcome="success"} 42\n',
                        encoding="utf-8")
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(ring), encoding="utf-8")
        verdict = tracker.run_driver(NODE, str(DRIVER), expo, ring_path, now)
        self.assertTrue(verdict["held"])
        self.assertFalse(verdict["slow"])
        self.assertFalse(verdict.get("slowCovered", True))

    def test_malformed_ring_rebuilds_silently(self):
        now = int(time.time())
        exposition, _ = tracker.fixture_inputs("fire", now)
        expo = self.dir / "expo.prom"
        expo.write_text(exposition, encoding="utf-8")
        ring_path = self.dir / "ring.json"
        ring_path.write_text("not json", encoding="utf-8")
        verdict = tracker.run_driver(NODE, str(DRIVER), expo, ring_path, now)
        self.assertFalse(verdict["slow"])
        self.assertFalse(verdict.get("slowCovered", True))


if __name__ == "__main__":
    unittest.main()

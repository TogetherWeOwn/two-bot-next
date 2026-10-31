"""Offline slow-burn tracker fixtures; stdlib only, no network access."""

import importlib
import json
import os
import shutil
import subprocess
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


def setUpModule():
    # main() defaults its ring, coverage and evidence paths to the cwd. Run
    # the module from private scratch so the suite never writes into the
    # checkout it was started from.
    scratch = tempfile.TemporaryDirectory(prefix="slow-burn-tracker-cwd-")
    previous = Path.cwd()
    os.chdir(scratch.name)
    unittest.addModuleCleanup(scratch.cleanup)
    unittest.addModuleCleanup(os.chdir, previous)


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
        self.bodies = []
        self.next_number = max([issue["number"] for issue in self.issues] + [0]) + 1

    def __call__(self, argv, **kwargs):
        self.calls.append(argv)
        if "--body-file" in argv:
            body_file = Path(argv[argv.index("--body-file") + 1])
            self.bodies.append(body_file.read_text(encoding="utf-8"))
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

    def test_unconfigured_fails_closed_naming_the_unset_binding(self):
        cases = (
            ({"STAGING_WORKER_URL": "", "METRICS_SCRAPE_TOKEN": ""},
             ("STAGING_WORKER_URL", "METRICS_SCRAPE_TOKEN")),
            ({"STAGING_WORKER_URL": "https://staging.example",
              "METRICS_SCRAPE_TOKEN": ""}, ("METRICS_SCRAPE_TOKEN",)),
            ({"STAGING_WORKER_URL": "",
              "METRICS_SCRAPE_TOKEN": "token"}, ("STAGING_WORKER_URL",)),
        )
        for extra, missing in cases:
            with self.subTest(missing=missing):
                evidence = str(self.dir / "evidence.json")
                with mock.patch("sys.stdout") as stdout:
                    with mock.patch.object(
                            tracker.urllib.request, "build_opener",
                            side_effect=AssertionError("must not scrape")):
                        with mock.patch(
                                "subprocess.run",
                                side_effect=AssertionError("gh must not run")):
                            code = tracker.main(
                                ["--evidence-out", evidence, "--exposition-out",
                                 str(self.dir / "expo.prom")],
                                base_env(extra))
                self.assertEqual(code, 1)
                out = stdout.write.call_args_list[0].args[0]
                self.assertIn("::error::", out)
                for name in missing:
                    self.assertIn(name, out)
                for name in set(extra) - set(missing):
                    self.assertNotIn(name, out)
                stored = json.loads(Path(evidence).read_text())
                self.assertEqual(stored["verdict"], "unconfigured")
                self.assertEqual(stored["action"], "none")

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

    def test_scrape_names_its_user_agent(self):
        seen = {}

        def fake_open(request, timeout=None):
            seen["agent"] = request.get_header("User-agent")
            seen["auth"] = request.get_header("Authorization")
            response = mock.MagicMock()
            response.status = 200
            response.read.return_value = b"# expo\n"
            response.__enter__.return_value = response
            return response

        opener = mock.Mock()
        opener.open.side_effect = fake_open
        with mock.patch.object(tracker.urllib.request, "build_opener",
                               return_value=opener):
            body = tracker.scrape_metrics("https://staging.example", "token")
        self.assertEqual(body, "# expo\n")
        self.assertEqual(seen["agent"], tracker.USER_AGENT)
        self.assertNotIn("Python-urllib", seen["agent"])
        self.assertEqual(seen["auth"], "Bearer token")

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

    def test_reconcile_writes_no_body_files_to_the_working_directory(self):
        # Regression: create, refresh and close used to write
        # tracker-*-body.md into the cwd, dirtying the checkout per run.
        now = 1_700_000_000
        cwd = self.dir / "cwd"
        cwd.mkdir()
        previous = Path.cwd()
        os.chdir(cwd)
        self.addCleanup(os.chdir, previous)
        stale = tracker.with_marker("firing", now - 7 * 3600)
        cases = [
            (FakeGh(), True, "created"),
            (FakeGh([{"number": 7, "title": tracker.issue_title(tracker.TITLE_PREFIX),
                      "body": stale}]), True, "refreshed"),
            (FakeGh([{"number": 7, "title": tracker.issue_title(tracker.TITLE_PREFIX),
                      "body": stale}]), False, "closed"),
        ]
        for fake, slow, expected in cases:
            with self.subTest(expected), mock.patch("subprocess.run", fake):
                action, _ = tracker.reconcile(
                    repo=REPO, token="t", label="slow-burn",
                    prefix=tracker.TITLE_PREFIX, origin="fixture:fire",
                    run_url=RUN_URL, now=now,
                    slow=slow, fast_also=False, dry_run=False)
                self.assertEqual(action, expected)
                self.assertTrue(fake.bodies)
                self.assertTrue(all(RUN_URL in body or "slow-burn-tracker" in body
                                    for body in fake.bodies))
                self.assertEqual(sorted(os.listdir(cwd)), [])

    def test_ignored_runtime_outputs_are_not_tracked(self):
        # A tracked copy of an ignored runtime output is rewritten by every
        # run, and a tracked coverage seed defeats the cache-miss warm-up.
        root = Path(__file__).resolve().parent.parent
        names = ["slow-burn-ring.json", "slow-burn-coverage.json",
                 "slow-burn-ring-prev.json", "slow-burn-tracker-evidence.json",
                 "slow-burn-exposition.prom", "tracker-refresh-body.md",
                 "tracker-issue-body.md", "tracker-resolve-body.md"]
        try:
            listed = subprocess.run(
                ["git", "-C", str(root), "ls-files", "--", *names],
                capture_output=True, text=True, timeout=30, check=True).stdout
        except (OSError, subprocess.SubprocessError):
            self.skipTest("git checkout unavailable")
        self.assertEqual(listed.split(), [])

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

    def test_ring_input_is_unique_temp_file_beside_exposition(self):
        # The driver input must be a per-invocation temp file beside the
        # exposition output, never the shared cwd sidecar: two runs see
        # different paths and leave no sidecar behind.
        evidence = str(self.dir / "evidence.json")
        expo = self.dir / "expo.prom"
        held = {"slow": False, "fast": False, "held": True, "ring": [],
                "slowCovered": False}
        sidecar = Path("slow-burn-ring-prev.json")
        sidecar_before = sidecar.read_bytes() if sidecar.exists() else None
        seen = []

        def capture(node, driver, exposition_path, ring_path, now):
            seen.append(Path(ring_path))
            self.assertTrue(Path(ring_path).is_file())
            return held

        argv = ["--fixture", "fire", "--evidence-out", evidence,
                "--exposition-out", str(expo)]
        with mock.patch.object(tracker, "run_driver", side_effect=capture):
            with mock.patch("subprocess.run",
                             side_effect=AssertionError("gh must not run")):
                self.assertEqual(tracker.main(list(argv), base_env()), 0)
                self.assertEqual(tracker.main(list(argv), base_env()), 0)
        self.assertEqual(len(seen), 2)
        self.assertNotEqual(seen[0], seen[1])
        for ring_path in seen:
            self.assertEqual(ring_path.parent.parent.resolve(),
                             expo.resolve().parent)
            self.assertTrue(
                ring_path.parent.name.startswith("slow-burn-ring-"))
            self.assertFalse(ring_path.exists())
            self.assertFalse(ring_path.parent.exists())
        leftovers = [p for p in expo.parent.iterdir()
                     if p.name.startswith("slow-burn-ring-")]
        self.assertEqual(leftovers, [])
        # The old shared-input path clobbered this tracked file mid-suite.
        if sidecar_before is None:
            self.assertFalse(sidecar.exists())
        else:
            self.assertEqual(sidecar.read_bytes(), sidecar_before)

    def test_ring_temp_input_cleaned_up_after_driver_error(self):
        evidence = str(self.dir / "evidence.json")
        expo = self.dir / "expo.prom"
        sidecar = Path("slow-burn-ring-prev.json")
        sidecar_before = sidecar.read_bytes() if sidecar.exists() else None
        seen = []

        def failing(node, driver, exposition_path, ring_path, now):
            seen.append(Path(ring_path))
            raise tracker.TrackError("burn evaluation failed: driver did not run")

        with mock.patch.object(tracker, "run_driver", side_effect=failing):
            code = tracker.main(
                ["--fixture", "fire", "--evidence-out", evidence,
                 "--exposition-out", str(expo)],
                base_env())
        self.assertEqual(code, 1)
        self.assertEqual(len(seen), 1)
        self.assertTrue(seen[0].parent.name.startswith("slow-burn-ring-"))
        self.assertFalse(seen[0].exists())
        self.assertFalse(seen[0].parent.exists())
        if sidecar_before is None:
            self.assertFalse(sidecar.exists())
        else:
            self.assertEqual(sidecar.read_bytes(), sidecar_before)

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

    def test_ring_gap_seconds(self):
        now = 1_700_000_000
        self.assertIsNone(tracker.ring_gap_seconds([], now))
        self.assertIsNone(tracker.ring_gap_seconds("not-a-list", now))
        ring = [{"t": now - 900, "received": 1, "duplicate": 0, "fastMoves": 1},
                {"t": now - 300, "received": 2, "duplicate": 0, "fastMoves": 2}]
        self.assertEqual(tracker.ring_gap_seconds(ring, now), 300)
        old = [{"t": now - 900, "received": 1, "duplicate": 0, "fastMoves": 0}]
        self.assertEqual(tracker.ring_gap_seconds(old, now), 900)

    def test_coarse_ring_gap_warns_and_stays_unknown(self):
        # GitHub runs */5 best-effort: a ring gap of 600 s or more must warn
        # instead of reading as a clean bill of burn health.
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(
            [{"t": now - 900, "received": 25, "duplicate": 0, "fastMoves": 24}]),
            encoding="utf-8")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        next_ring = [{"t": now, "received": 50, "duplicate": 0, "fastMoves": 48}]
        uncovered = {"slow": False, "fast": False, "held": False,
                     "ring": next_ring, "slowCovered": False}
        writes = []
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=uncovered):
                with mock.patch("sys.stdout") as stdout:
                    with mock.patch.object(tracker.time, "time", return_value=now):
                        code = tracker.main(
                            ["--ring", str(ring_path), "--evidence-out", evidence,
                             "--exposition-out", str(self.dir / "expo.prom")],
                            env)
                    for call in stdout.write.call_args_list:
                        writes.append(call.args[0])
        self.assertEqual(code, 0)
        self.assertTrue(any("::warning::" in chunk and "coarse" in chunk
                            for chunk in writes),
                        f"expected a coarse-cadence warning, got {writes!r}")
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["cadence"], "coarse")
        self.assertEqual(stored["stale_gap_seconds"], 900)

    def test_fresh_ring_has_no_cadence_warning(self):
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(
            [{"t": now - 300, "received": 25, "duplicate": 0, "fastMoves": 24}]),
            encoding="utf-8")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        next_ring = [{"t": now, "received": 50, "duplicate": 0, "fastMoves": 48}]
        uncovered = {"slow": False, "fast": False, "held": False,
                     "ring": next_ring, "slowCovered": False}
        writes = []
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=uncovered):
                with mock.patch("sys.stdout") as stdout:
                    with mock.patch.object(tracker.time, "time", return_value=now):
                        code = tracker.main(
                            ["--ring", str(ring_path), "--evidence-out", evidence,
                             "--exposition-out", str(self.dir / "expo.prom")],
                            env)
                    for call in stdout.write.call_args_list:
                        writes.append(call.args[0])
        self.assertEqual(code, 0)
        self.assertFalse(any("::warning::" in chunk for chunk in writes),
                         f"fresh ring must not warn, got {writes!r}")
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["cadence"], "ok")

    def test_coverage_sidecar_round_trip_and_rejects_junk(self):
        now = 1_700_000_000
        path = self.dir / "coverage.json"
        self.assertIsNone(tracker.load_covered_at(str(path)))
        tracker.save_covered_at(str(path), now)
        self.assertEqual(tracker.load_covered_at(str(path)), now)
        for junk in ("not json", "[1, 2]",
                     json.dumps({"last_covered": "soon"}),
                     json.dumps({"last_covered": True}),
                     json.dumps({"last_covered": 0}),
                     json.dumps({"last_covered": -5}),
                     json.dumps({"other": now})):
            path.write_text(junk, encoding="utf-8")
            self.assertIsNone(tracker.load_covered_at(str(path)),
                              f"sidecar must reject {junk!r}")
        path.write_text(json.dumps({"last_covered": now - 3600}),
                        encoding="utf-8")
        self.assertEqual(tracker.load_covered_at(str(path)), now - 3600)
        path.write_text(str(now - 7200), encoding="utf-8")
        self.assertEqual(tracker.load_covered_at(str(path)), now - 7200)

    def test_covered_verdict_persists_the_moment(self):
        # Every covered run (fire or clear) refreshes the sidecar, so a
        # later unknown run can measure how long coverage has been gone.
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(
            [{"t": now - 300, "received": 25, "duplicate": 0, "fastMoves": 24}]),
            encoding="utf-8")
        coverage_path = str(self.dir / "coverage.json")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        next_ring = [{"t": now, "received": 50, "duplicate": 0, "fastMoves": 50}]
        covered = {"slow": False, "fast": False, "held": False,
                   "ring": next_ring, "slowCovered": True}
        fake = FakeGh()
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=covered):
                with mock.patch("subprocess.run", fake):
                    with mock.patch.object(tracker.time, "time", return_value=now):
                        code = tracker.main(
                            ["--ring", str(ring_path), "--coverage-path",
                             coverage_path, "--evidence-out", evidence,
                             "--exposition-out", str(self.dir / "expo.prom")],
                            env)
        self.assertEqual(code, 0)
        self.assertEqual(tracker.load_covered_at(coverage_path), now)
        self.assertEqual(json.loads(Path(ring_path).read_text()), next_ring)
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "clear")

    def test_sub_600s_gap_with_stale_coverage_warns(self):
        # The review repro: 420 s spacings never trip the entry-age check,
        # yet the baseline window stays empty. A 7 h-old covered moment must
        # still warn and mark the run coarse instead of reading green.
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(
            [{"t": now - 420, "received": 25, "duplicate": 0, "fastMoves": 24}]),
            encoding="utf-8")
        coverage_path = str(self.dir / "coverage.json")
        Path(coverage_path).write_text(
            json.dumps({"last_covered": now - 7 * 3600}), encoding="utf-8")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        next_ring = [{"t": now, "received": 50, "duplicate": 0, "fastMoves": 48}]
        uncovered = {"slow": False, "fast": False, "held": False,
                     "ring": next_ring, "slowCovered": False}
        writes = []
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=uncovered):
                with mock.patch("sys.stdout") as stdout:
                    with mock.patch.object(tracker.time, "time", return_value=now):
                        code = tracker.main(
                            ["--ring", str(ring_path), "--coverage-path",
                             coverage_path, "--evidence-out", evidence,
                             "--exposition-out", str(self.dir / "expo.prom")],
                            env)
                    for call in stdout.write.call_args_list:
                        writes.append(call.args[0])
        self.assertEqual(code, 0)
        self.assertTrue(any("::warning::" in chunk and "no covered" in chunk
                            for chunk in writes),
                        f"expected a stale-coverage warning, got {writes!r}")
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["cadence"], "coarse")
        self.assertEqual(stored["uncovered_seconds"], 7 * 3600)
        self.assertEqual(stored["stale_gap_seconds"], 420)

    def test_recently_covered_unknown_stays_quiet(self):
        # Coverage an hour ago means the baseline only just lapsed: record
        # the gap, but do not warn yet.
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(
            [{"t": now - 420, "received": 25, "duplicate": 0, "fastMoves": 24}]),
            encoding="utf-8")
        coverage_path = str(self.dir / "coverage.json")
        Path(coverage_path).write_text(
            json.dumps({"last_covered": now - 3600}), encoding="utf-8")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        next_ring = [{"t": now, "received": 50, "duplicate": 0, "fastMoves": 48}]
        uncovered = {"slow": False, "fast": False, "held": False,
                     "ring": next_ring, "slowCovered": False}
        writes = []
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=uncovered):
                with mock.patch("sys.stdout") as stdout:
                    with mock.patch.object(tracker.time, "time", return_value=now):
                        code = tracker.main(
                            ["--ring", str(ring_path), "--coverage-path",
                             coverage_path, "--evidence-out", evidence,
                             "--exposition-out", str(self.dir / "expo.prom")],
                            env)
                    for call in stdout.write.call_args_list:
                        writes.append(call.args[0])
        self.assertEqual(code, 0)
        self.assertFalse(any("::warning::" in chunk for chunk in writes),
                         f"recent coverage must not warn, got {writes!r}")
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["cadence"], "ok")
        self.assertEqual(stored["uncovered_seconds"], 3600)

    def drive_unknown_loop(self, step, runs, start=1_700_000_000):
        """Drive live unknown runs from an empty ring with no sidecar.

        Mocks the scrape and the verdict (always uncovered) so the loop
        proves the tracker warning, not the evaluator: the evaluator pins
        that 420/590 s spacings stay uncovered in the driver tests below.
        Returns (warn_chunks, last_evidence, ring_path, coverage_path).
        """
        ring_path = self.dir / "ring.json"
        coverage_path = self.dir / "coverage.json"
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})

        def uncovered_driver(*args, **kwargs):
            tick = int(tracker.time.time())
            return {"slow": False, "fast": False, "held": False,
                    "ring": [{"t": tick, "received": 50,
                              "duplicate": 0, "fastMoves": 48}],
                    "slowCovered": False}

        warn_chunks = []
        last_writes = []
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver",
                                   side_effect=uncovered_driver):
                for index in range(runs):
                    tick = start + index * step
                    with mock.patch.object(tracker.time, "time",
                                            return_value=tick):
                        with mock.patch("sys.stdout") as stdout:
                            code = tracker.main(
                                ["--ring", str(ring_path), "--coverage-path",
                                 str(coverage_path), "--evidence-out", evidence,
                                 "--exposition-out",
                                 str(self.dir / "expo.prom")],
                                env)
                    self.assertEqual(code, 0)
                    last_writes = [call.args[0] for call in
                                   stdout.write.call_args_list]
                    warn_chunks = [chunk for chunk in last_writes
                                   if "::warning::" in chunk]
        stored = json.loads(Path(evidence).read_text())
        return warn_chunks, stored, ring_path, coverage_path

    def test_never_covered_420s_loop_warns_after_warmup(self):
        # The review repro: steady 420 s from an empty ring never covers,
        # so the first seed must still warn once ~6.5 h have passed.
        warns, stored, _, coverage_path = self.drive_unknown_loop(
            step=420, runs=60)
        self.assertTrue(
            any("no covered" in chunk for chunk in warns),
            f"steady 420 s from empty must warn, got {warns!r}")
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["cadence"], "coarse")
        self.assertGreaterEqual(stored["uncovered_seconds"],
                                tracker.UNCOVERED_WARN_SECONDS)
        seeded = json.loads(Path(coverage_path).read_text())
        self.assertEqual(seeded.get("tracking_since"), 1_700_000_000)

    def test_never_covered_590s_loop_warns_after_warmup(self):
        # Just under the 600 s entry-age tripwire: same silence at first,
        # then the same seeded warning once the install is ~6.5 h old.
        warns, stored, _, coverage_path = self.drive_unknown_loop(
            step=590, runs=45)
        self.assertTrue(
            any("no covered" in chunk for chunk in warns),
            f"steady 590 s from empty must warn, got {warns!r}")
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["cadence"], "coarse")
        self.assertGreaterEqual(stored["uncovered_seconds"],
                                tracker.UNCOVERED_WARN_SECONDS)
        seeded = json.loads(Path(coverage_path).read_text())
        self.assertEqual(seeded.get("tracking_since"), 1_700_000_000)

    def test_healthy_300s_warmup_stays_quiet_before_threshold(self):
        # Warm-up takes ~6 h: 5 h of 300 s unknowns must not warn yet, so
        # a healthy install keeps its margin before first coverage lands.
        warns, stored, _, _ = self.drive_unknown_loop(step=300, runs=60)
        self.assertEqual(warns, [])
        self.assertEqual(stored["verdict"], "unknown")
        self.assertEqual(stored["cadence"], "ok")
        self.assertLess(stored["uncovered_seconds"],
                        tracker.UNCOVERED_WARN_SECONDS)

    def test_tracking_seed_preserves_covered_moment(self):
        # Seeding must not clobber an existing covered moment, and a covered
        # run must keep the tracking start while refreshing the cover time.
        now = 1_700_000_000
        path = self.dir / "coverage.json"
        path.write_text(json.dumps({"last_covered": now - 3600}),
                        encoding="utf-8")
        tracker.seed_tracking_since(str(path), now)
        self.assertEqual(tracker.load_covered_at(str(path)), now - 3600)
        self.assertIsNone(tracker.load_tracking_since(str(path)))
        tracker.save_covered_at(str(path), now)
        fresh = json.loads(path.read_text())
        self.assertEqual(fresh.get("last_covered"), now)
        blank = self.dir / "fresh.json"
        tracker.seed_tracking_since(str(blank), now)
        self.assertIsNone(tracker.load_covered_at(str(blank)))
        self.assertEqual(tracker.load_tracking_since(str(blank)), now)
        tracker.save_covered_at(str(blank), now + 60)
        self.assertEqual(tracker.load_covered_at(str(blank)), now + 60)
        self.assertEqual(tracker.load_tracking_since(str(blank)), now)

    def test_held_verdict_leaves_coverage_alone(self):
        # Unusable join series decide nothing: the covered moment must
        # survive a held run untouched.
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(
            [{"t": now - 300, "received": 25, "duplicate": 0, "fastMoves": 24}]),
            encoding="utf-8")
        coverage_path = self.dir / "coverage.json"
        coverage_path.write_text(json.dumps({"last_covered": now - 3600}),
                                 encoding="utf-8")
        evidence = str(self.dir / "evidence.json")
        env = base_env({"STAGING_WORKER_URL": "https://staging.example",
                        "METRICS_SCRAPE_TOKEN": "token"})
        held = {"slow": False, "fast": False, "held": True, "ring": [],
                "slowCovered": False}
        with mock.patch.object(tracker, "scrape_metrics", return_value="expo"):
            with mock.patch.object(tracker, "run_driver", return_value=held):
                with mock.patch("subprocess.run",
                                 side_effect=AssertionError("gh must not run")):
                    with mock.patch.object(tracker.time, "time", return_value=now):
                        code = tracker.main(
                            ["--ring", str(ring_path), "--coverage-path",
                             str(coverage_path), "--evidence-out", evidence,
                             "--exposition-out", str(self.dir / "expo.prom")],
                            env)
        self.assertEqual(code, 0)
        stored = json.loads(Path(evidence).read_text())
        self.assertEqual(stored["verdict"], "held")
        self.assertEqual(tracker.load_covered_at(str(coverage_path)), now - 3600)

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
    @classmethod
    def setUpClass(cls):
        # Hand-built cadence fixtures need every companion series a Worker
        # scrape requires; keep fixture_inputs itself under test unchanged.
        fixture = (DRIVER.parent.parent / "test" / "fixtures" / "metrics.ts").as_uri()
        import subprocess
        completed = subprocess.run(
            [NODE, "--input-type=module", "-e",
             f"import {{ metricsLines }} from {json.dumps(fixture)};"
             'process.stdout.write(metricsLines().filter(line => '
             '!line.startsWith(\'two_bot_voice_joins_received_total \') && '
             '!line.startsWith(\'two_bot_voice_join_refused_total{reason="duplicate"} \') && '
             '!line.startsWith(\'two_bot_voice_join_to_move_seconds_bucket{le="5"} \')'
             ').join("\\n") + "\\n");'],
            capture_output=True, text=True, timeout=10, check=True)
        cls.companion_exposition = completed.stdout

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

    def test_coarse_600s_steps_stay_uncovered(self):
        # Pins the load-bearing cadence: GitHub runs */5 best-effort, and at
        # 600 s+ steps the slow long-window baseline is already pruned, so a
        # sustained 8x burn never covers. The tracker must warn (unit-tested
        # above) instead of filing or clearing on such a ring.
        now = int(time.time())
        step = 600
        snapshots = 48
        ring = []
        received = 0
        fast = 0
        for index in range(snapshots):
            received += 25
            fast += 24
            ring.append({"t": now - (snapshots - index) * step,
                         "received": received, "duplicate": 0,
                         "fastMoves": fast})
        received += 25
        fast += 24
        exposition = self.companion_exposition + (
            f"two_bot_voice_joins_received_total {received}\n"
            f'two_bot_voice_join_refused_total{{reason="duplicate"}} 0\n'
            f'two_bot_voice_join_to_move_seconds_bucket{{le="5"}} {fast}\n'
        )
        expo = self.dir / "expo.prom"
        expo.write_text(exposition, encoding="utf-8")
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(ring), encoding="utf-8")
        verdict = tracker.run_driver(NODE, str(DRIVER), expo, ring_path, now)
        self.assertFalse(verdict["slow"])
        self.assertFalse(verdict.get("slowCovered", True))
        self.assertGreaterEqual(
            tracker.ring_gap_seconds(ring, now), 600)

    def run_burn_loop(self, step_fn, snapshots):
        """Replay a sustained 8x burn at the given per-step spacings."""
        now = int(time.time())
        stamps = [now]
        for index in range(snapshots):
            stamps.append(stamps[-1] - step_fn(index))
        stamps = sorted(stamps)[:-1]
        ring = []
        received = 0
        fast = 0
        for tick in stamps:
            received += 25
            fast += 24
            ring.append({"t": tick, "received": received, "duplicate": 0,
                         "fastMoves": fast})
        received += 25
        fast += 24
        exposition = self.companion_exposition + (
            f"two_bot_voice_joins_received_total {received}\n"
            f'two_bot_voice_join_refused_total{{reason="duplicate"}} 0\n'
            f'two_bot_voice_join_to_move_seconds_bucket{{le="5"}} {fast}\n'
        )
        expo = self.dir / "expo.prom"
        expo.write_text(exposition, encoding="utf-8")
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(ring), encoding="utf-8")
        return tracker.run_driver(NODE, str(DRIVER), expo, ring_path, now)

    def test_steady_420s_steps_stay_uncovered(self):
        # Below the 600 s entry-age tripwire, so the old check stays silent,
        # yet the narrow baseline window still holds no snapshot: a sustained
        # 8x burn covers nothing and fires nothing.
        verdict = self.run_burn_loop(lambda index: 420, 60)
        self.assertFalse(verdict["held"])
        self.assertFalse(verdict["slow"])
        self.assertFalse(verdict.get("slowCovered", True))

    def test_steady_590s_steps_stay_uncovered(self):
        # Just under the 600 s tripwire: same silence, same missing baseline.
        verdict = self.run_burn_loop(lambda index: 590, 48)
        self.assertFalse(verdict["held"])
        self.assertFalse(verdict["slow"])
        self.assertFalse(verdict.get("slowCovered", True))

    def test_alternating_300s_560s_steps_stay_uncovered(self):
        # A jittered cadence averaging ~430 s also starves the baseline
        # window: no fire, no coverage, no warning from the entry-age check.
        verdict = self.run_burn_loop(
            lambda index: 300 if index % 2 == 0 else 560, 60)
        self.assertFalse(verdict["held"])
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


class TrackerCacheDuplicateKeyTests(unittest.TestCase):
    """Duplicate keys in the cached ring/sidecar rebuild, like other malformed cache data."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def test_duplicate_key_ring_entry_rebuilds(self):
        now = 1_700_000_000
        ring_path = self.dir / "ring.json"
        ring_path.write_text(
            '[{"t": 100, "t": 9999999999, "received": 25,'
            ' "duplicate": 0, "fastMoves": 24}]', encoding="utf-8")
        ring = tracker.load_ring(str(ring_path))
        self.assertEqual(ring, [])
        self.assertIsNone(tracker.ring_gap_seconds(ring, now))

    def test_duplicate_key_sidecar_rebuilds(self):
        sidecar = self.dir / "coverage.json"
        sidecar.write_text('{"last_covered": 100, "last_covered": 9999999999}',
                           encoding="utf-8")
        self.assertEqual(tracker._load_sidecar_dict(str(sidecar)), {})
        self.assertIsNone(tracker.load_covered_at(str(sidecar)))

    def test_unique_cache_still_loads_byte_identical(self):
        ring = [{"t": 1_699_999_700, "received": 25,
                 "duplicate": 0, "fastMoves": 24}]
        ring_path = self.dir / "ring.json"
        ring_path.write_text(json.dumps(ring), encoding="utf-8")
        self.assertEqual(tracker.load_ring(str(ring_path)), ring)
        sidecar = self.dir / "coverage.json"
        sidecar.write_text('{"last_covered": 1699999700}', encoding="utf-8")
        self.assertEqual(tracker.load_covered_at(str(sidecar)), 1699999700)


# Keep protected intake acceptance tests in the existing CI entrypoint.
from test_slow_burn_tracker_intake_acceptance import TrackerIntakeAcceptanceTests
from test_slow_burn_tracker_scratch_acceptance import TrackerScratchAcceptanceTests


if __name__ == "__main__":
    unittest.main()

"""Offline pins for scripts/promotion_regression_gate.py (no network, no secrets)."""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first.
import promotion_regression_gate as gate

SHA = "0123456789abcdef0123456789abcdef01234567"
NEWER = "f" * 40


def probe(revision=SHA, build_id="1-1", **verdicts):
    names = {"health": "pass", "readyz": "pass", "build": "pass", **verdicts}
    return {"origin": "https://example.test", "build_revision": revision, "build_id": build_id,
            "checks": [{"check": name, "verdict": verdict, "shape": "detail"}
                       for name, verdict in names.items()],
            "result": "pass" if all(v == "pass" for v in names.values()) else "fail"}


def rest(errors, total, incarnation=None):
    receipt = {"available": True, "code": "scraped", "rest5xx": errors, "restTotal": total}
    if incarnation is not None:
        receipt["incarnation"] = incarnation
    return receipt


OLD_INC = "a" * 32
NEW_INC = "b" * 32


def runs(*rows):
    return list(rows)


def run_row(head_sha, status, conclusion=None, branch="main"):
    return {"head_sha": head_sha, "head_branch": branch, "status": status, "conclusion": conclusion}


class StagingStepTests(unittest.TestCase):
    def test_candidate_on_staging_records_its_journeys_and_counters(self):
        skip, evidence = gate.staging_step(probe(), rest(1, 200), SHA, auto=True)
        self.assertEqual(skip, "")
        self.assertEqual(evidence, {"sha": SHA, "staging_build_revision": SHA, "staging_build_id": "1-1",
                                    "expected_build_id": None,
                                    "checks": {"health": "pass", "readyz": "pass", "build": "pass"},
                                    "rest": {"rest5xx": 1, "restTotal": 200}, "baseline": None,
                                    "rest_code": "scraped"})

    def test_the_guarded_run_attempt_binds_the_probe_build(self):
        # Same SHA, guarded attempt: only that build proves the candidate soaked.
        skip, evidence = gate.staging_step(probe(), rest(1, 200), SHA, auto=True,
                                           expected_build_id="1-1",
                                           staging_runs=runs(run_row(SHA, "in_progress")))
        self.assertEqual(skip, "")
        self.assertEqual(evidence["staging_build_id"], "1-1")
        skip, _ = gate.staging_step(probe(), rest(1, 200), SHA, auto=True,
                                    expected_build_id="2-5",
                                    staging_runs=runs(run_row(SHA, "completed", "success")))
        self.assertEqual(skip, "staging-superseded")
        with self.assertRaises(gate.GateError):
            gate.staging_step(probe(), rest(1, 200), SHA, auto=False, expected_build_id="2-5",
                              staging_runs=runs(run_row(SHA, "completed", "success")))
        # An unstamped build never matches a guarded attempt.
        with self.assertRaises(gate.GateError):
            gate.staging_step(probe(build_id="unknown"), rest(1, 200), SHA, auto=True,
                              expected_build_id="1-1", staging_runs=[])

    def test_a_serving_build_nothing_promotes_refuses_a_stalled_promotion(self):
        moved = probe(revision=NEWER)
        stalled = (None, runs(run_row(NEWER, "completed", "failure")),
                   runs(run_row(NEWER, "completed", "failure", branch="feature")),
                   runs(run_row(NEWER, "waiting")), runs(run_row(NEWER, None)))
        for staging_runs in stalled:
            with self.subTest(staging_runs=staging_runs):
                with self.assertRaises(gate.GateError) as raised:
                    gate.staging_step(moved, rest(0, 50), SHA, auto=True,
                                      expected_build_id="1-1", staging_runs=staging_runs)
                self.assertIn("stalled", str(raised.exception))
        for staging_runs in (runs(run_row(NEWER, "in_progress")),
                             runs(run_row(NEWER, "queued")),
                             runs(run_row(NEWER, "completed", "success")),
                             runs(run_row(NEWER, "completed", "failure"),
                                  run_row(NEWER, "completed", "success"))):
            with self.subTest(staging_runs=staging_runs):
                skip, _ = gate.staging_step(moved, rest(0, 50), SHA, auto=True,
                                            expected_build_id="1-1", staging_runs=staging_runs)
                self.assertEqual(skip, "staging-superseded")

    def test_a_miswired_run_identity_refuses_instead_of_dropping_the_binding(self):
        with self.assertRaises(gate.GateError):
            gate.expected_build_id("abc", "")
        self.assertIsNone(gate.expected_build_id("", ""))

    def test_superseded_staging_skips_automatic_runs_and_refuses_manual_ones(self):
        # A newer build a staging run still promotes: the newer build promotes itself.
        promoting = runs(run_row(NEWER, "in_progress"))
        skip, _ = gate.staging_step(probe(revision=NEWER), rest(0, 50), SHA, auto=True,
                                    staging_runs=promoting)
        self.assertEqual(skip, "staging-superseded")
        with self.assertRaises(gate.GateError):
            gate.staging_step(probe(revision=NEWER), rest(0, 50), SHA, auto=False,
                              staging_runs=promoting)

    def test_a_missing_probe_receipt_never_skips(self):
        with self.assertRaises(gate.GateError):
            gate.staging_step(None, rest(0, 50), SHA, auto=True)

    def test_an_unreadable_staging_revision_refuses_instead_of_skipping(self):
        # A down or unstamped staging build is not proof of a newer build.
        for revision in (None, "unknown", "", SHA.upper()):
            with self.subTest(revision=revision):
                receipt = probe(revision=revision, health="fail", readyz="fail", build="fail")
                with self.assertRaises(gate.GateError):
                    gate.staging_step(receipt, rest(0, 50), SHA, auto=True)

    def test_unavailable_or_inconsistent_counters_record_no_data(self):
        for counters in (None, {"available": False, "code": "token_absent"},
                         rest(5, 4), {**rest(1, 10), "rest5xx": True}):
            with self.subTest(counters=counters):
                _, evidence = gate.staging_step(probe(), counters, SHA, auto=True)
                self.assertIsNone(evidence["rest"])


def staged(checks=None, counters=(0, 200), incarnation=None, baseline="absent"):
    def block(pair):
        if pair is None:
            return None
        return {"rest5xx": pair[0], "restTotal": pair[1],
                **({"incarnation": incarnation} if incarnation else {})}
    evidence = {"sha": SHA, "checks": checks or {"health": "pass", "readyz": "pass", "build": "pass"},
                "rest": block(counters)}
    if baseline != "absent":
        evidence["baseline"] = block(baseline)
    return evidence


def bookended(checks=None, counters=(0, 200), incarnation=OLD_INC):
    return staged(checks, counters, incarnation=incarnation, baseline=counters)


class CompareTests(unittest.TestCase):
    def test_no_regression_passes(self):
        evidence = gate.compare(staged(), probe(revision=NEWER), rest(2, 1000))
        self.assertEqual((evidence["verdict"], evidence["code"]), ("pass", "no_regression"))
        self.assertEqual(evidence["rest_5xx"]["baseline"], "available")

    def test_a_journey_passing_on_production_but_failing_on_staging_refuses(self):
        evidence = gate.compare(staged({"health": "pass", "readyz": "fail", "build": "pass"}),
                                probe(), rest(0, 1000))
        self.assertEqual((evidence["verdict"], evidence["code"]), ("fail", "journey_regression"))
        self.assertEqual(evidence["journeys"]["regressions"], ["readyz"])

    def test_a_journey_already_failing_on_production_is_not_a_regression(self):
        evidence = gate.compare(staged({"health": "pass", "readyz": "fail", "build": "pass"}),
                                probe(readyz="fail"), rest(0, 1000))
        self.assertEqual(evidence["verdict"], "pass")

    def test_a_journey_missing_on_staging_counts_as_a_regression(self):
        evidence = gate.compare(staged({"health": "pass"}), probe(), rest(0, 1000))
        self.assertEqual(evidence["journeys"]["regressions"], ["build", "readyz"])

    def test_5xx_ratio_against_the_production_baseline(self):
        # Production 10% -> threshold 15%: staging 14% passes, 16% refuses.
        self.assertEqual(gate.compare(staged(counters=(14, 100)), probe(), rest(100, 1000))
                         ["rest_5xx"]["verdict"], "pass")
        evidence = gate.compare(staged(counters=(16, 100)), probe(), rest(100, 1000))
        self.assertEqual((evidence["verdict"], evidence["code"]), ("fail", "rest_5xx_regression"))
        self.assertEqual(evidence["rest_5xx"]["threshold"], 0.15)

    def test_the_absolute_floor_applies_when_production_is_clean(self):
        # Production 0%: the 5% floor, not 1.5 x 0, decides.
        self.assertEqual(gate.compare(staged(counters=(4, 100)), probe(), rest(0, 1000))
                         ["rest_5xx"]["verdict"], "pass")
        self.assertEqual(gate.compare(staged(counters=(6, 100)), probe(), rest(0, 1000))
                         ["rest_5xx"]["verdict"], "fail")

    def test_too_few_staging_5xx_never_refuse(self):
        # 2 of 20 is 10% but below the 3-response minimum: noise, not a regression.
        self.assertEqual(gate.compare(staged(counters=(2, 20)), probe(), rest(0, 1000))
                         ["rest_5xx"]["verdict"], "pass")

    def test_quiet_or_unscraped_staging_is_insufficient_data_never_pass(self):
        for counters in ((0, 9), None):
            with self.subTest(counters=counters):
                evidence = gate.compare(staged(counters=counters), probe(), rest(0, 1000))
                self.assertEqual(evidence["rest_5xx"]["verdict"], "insufficient_data")
                self.assertEqual(evidence["verdict"], "pass")

    def test_a_thin_or_missing_production_baseline_falls_back_to_the_floor(self):
        for baseline in (rest(9, 9), {"available": False, "code": "token_absent"}, None):
            with self.subTest(baseline=baseline):
                evidence = gate.compare(staged(counters=(6, 100)), probe(), baseline)
                self.assertEqual(evidence["rest_5xx"]["baseline"], "floor_only")
                self.assertEqual(evidence["rest_5xx"]["threshold"], 0.05)
                self.assertEqual(evidence["rest_5xx"]["verdict"], "fail")


class SoakCoverageTests(unittest.TestCase):
    def test_a_bookended_window_judges_the_ratio(self):
        evidence = gate.compare(bookended(counters=(16, 100)), probe(), rest(2, 1000))
        self.assertEqual((evidence["verdict"], evidence["code"]), ("fail", "rest_5xx_regression"))
        self.assertEqual(evidence["rest_5xx"]["coverage"], "bookended")

    def test_a_restart_between_baseline_and_sample_is_insufficient_never_a_pass(self):
        # 50 post-restart 5xx of 100 would refuse on a bookended window; the
        # restart means the soak's own errors may have vanished with it.
        staging = bookended(counters=(50, 100))
        staging["baseline"] = {"rest5xx": 20, "restTotal": 1000, "incarnation": NEW_INC}
        staging["rest"]["incarnation"] = OLD_INC
        evidence = gate.compare(staging, probe(), rest(0, 1000))
        self.assertEqual(evidence["rest_5xx"]["coverage"], "incarnation_changed")
        self.assertEqual(evidence["rest_5xx"]["verdict"], "insufficient_data")
        self.assertEqual(evidence["verdict"], "pass")
        # Journeys still judge: a restart hides counters, not a broken build.
        staging = bookended({"health": "fail"}, counters=(50, 100))
        staging["baseline"] = {"rest5xx": 20, "restTotal": 1000, "incarnation": NEW_INC}
        staging["rest"]["incarnation"] = OLD_INC
        evidence = gate.compare(staging, probe(), rest(0, 1000))
        self.assertEqual((evidence["verdict"], evidence["code"]), ("fail", "journey_regression"))

    def test_a_missing_baseline_or_identity_names_its_coverage(self):
        evidence = gate.compare(staged(counters=(6, 100)), probe(), rest(0, 1000))
        self.assertEqual(evidence["rest_5xx"]["coverage"], "baseline_unavailable")
        evidence = gate.compare(bookended(counters=(6, 100), incarnation=None), probe(), rest(0, 1000))
        self.assertEqual(evidence["rest_5xx"]["coverage"], "identity_unavailable")


class MainTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.dir = Path(self.tmp.name)
        self.output = self.dir / "output"
        self.summary = self.dir / "summary"

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, name, value):
        path = self.dir / name
        path.write_text(value if isinstance(value, str) else json.dumps(value))
        return str(path)

    def run_main(self, argv):
        stdout = io.StringIO()
        env = {"GITHUB_OUTPUT": str(self.output), "GITHUB_STEP_SUMMARY": str(self.summary)}
        with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(stdout):
            code = gate.main(argv)
        return code, stdout.getvalue()

    def staging_args(self, revision=SHA, auto="true", extra=()):
        return ["staging", "--probe", self.write("probe.json", probe(revision=revision)),
                "--counters", self.write("counters.json", rest(0, 100)), "--sha", SHA,
                "--auto", auto, *extra, "--out", str(self.dir / "staging.json")]

    def test_staging_step_writes_outputs_for_the_production_job(self):
        code, _ = self.run_main(self.staging_args())
        self.assertEqual(code, 0)
        outputs = dict(line.split("=", 1) for line in self.output.read_text().splitlines())
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(json.loads(outputs["evidence"])["sha"], SHA)

    def test_staging_step_skips_or_refuses_a_superseded_candidate(self):
        promoting = self.write("runs.json", runs(run_row(NEWER, "completed", "success")))
        extra = ["--run-id", "1", "--attempt", "1", "--staging-runs", promoting]
        code, _ = self.run_main(self.staging_args(revision=NEWER, extra=extra))
        self.assertEqual(code, 0)
        self.assertIn("skip=staging-superseded", self.output.read_text())
        self.assertIn("staging-superseded", self.summary.read_text())
        code, stdout = self.run_main(self.staging_args(revision=NEWER, auto="false", extra=extra))
        self.assertEqual(code, 1)
        self.assertIn("::error::Refusing promotion", stdout)

    def test_staging_step_refuses_a_stalled_promotion_on_automatic_runs(self):
        stalled = self.write("runs.json", runs(run_row(NEWER, "completed", "failure")))
        extra = ["--run-id", "1", "--attempt", "1", "--staging-runs", stalled]
        code, stdout = self.run_main(self.staging_args(revision=NEWER, extra=extra))
        self.assertEqual(code, 1)
        self.assertIn("stalled", stdout)

    def test_staging_step_refuses_a_miswired_run_identity(self):
        code, stdout = self.run_main(self.staging_args(extra=["--run-id", "abc"]))
        self.assertEqual(code, 1)
        self.assertIn("run-attempt build", stdout)

    def test_staging_step_records_the_soak_baseline_for_the_compare(self):
        args = self.staging_args()
        baseline = self.write("baseline.json", rest(0, 50, incarnation=OLD_INC))
        self.write("counters.json", rest(1, 200, incarnation=OLD_INC))
        args += ["--baseline-counters", baseline]
        code, _ = self.run_main(args)
        self.assertEqual(code, 0)
        outputs = dict(line.split("=", 1) for line in self.output.read_text().splitlines())
        evidence = json.loads(outputs["evidence"])
        self.assertEqual(evidence["baseline"], {"rest5xx": 0, "restTotal": 50, "incarnation": OLD_INC})
        self.assertEqual(evidence["rest"]["incarnation"], OLD_INC)

    def compare_args(self, staging, prod=None):
        return ["compare", "--staging-evidence", json.dumps(staging),
                "--probe", self.write("prod.json", prod or probe(revision=NEWER)),
                "--counters", self.write("prod-counters.json", rest(0, 1000)),
                "--sha", SHA, "--out", str(self.dir / "regression.json")]

    def test_compare_passes_and_summarises(self):
        code, _ = self.run_main(self.compare_args(staged()))
        self.assertEqual(code, 0)
        self.assertIn("Regression vs production: `no_regression`", self.summary.read_text())

    def test_compare_refuses_a_regression(self):
        code, stdout = self.run_main(self.compare_args(staged(counters=(50, 100))))
        self.assertEqual(code, 1)
        self.assertIn("rest_5xx_regression", stdout)

    def test_compare_refuses_evidence_for_another_sha_or_malformed_input(self):
        for raw in (json.dumps({**staged(), "sha": NEWER}), "not json", "[]",
                    '{"sha": "x", "sha": "y"}'):
            with self.subTest(raw=raw):
                args = self.compare_args(staged())
                args[args.index("--staging-evidence") + 1] = raw
                code, stdout = self.run_main(args)
                self.assertEqual(code, 1)
                self.assertIn("cannot judge", stdout)

    def test_receipts_never_carry_probe_shapes_or_origins(self):
        self.run_main(self.compare_args(staged()))
        text = (self.dir / "regression.json").read_text() + self.summary.read_text()
        self.assertNotIn("example.test", text)
        self.assertNotIn("detail", text)


if __name__ == "__main__":
    unittest.main()

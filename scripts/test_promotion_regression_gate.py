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


def probe(revision=SHA, **verdicts):
    names = {"health": "pass", "readyz": "pass", "build": "pass", **verdicts}
    return {"origin": "https://example.test", "build_revision": revision, "build_id": "1-1",
            "checks": [{"check": name, "verdict": verdict, "shape": "detail"}
                       for name, verdict in names.items()],
            "result": "pass" if all(v == "pass" for v in names.values()) else "fail"}


def rest(errors, total):
    return {"available": True, "code": "scraped", "rest5xx": errors, "restTotal": total}


class StagingStepTests(unittest.TestCase):
    def test_candidate_on_staging_records_its_journeys_and_counters(self):
        skip, evidence = gate.staging_step(probe(), rest(1, 200), SHA, auto=True)
        self.assertEqual(skip, "")
        self.assertEqual(evidence, {"sha": SHA, "staging_build_revision": SHA,
                                    "checks": {"health": "pass", "readyz": "pass", "build": "pass"},
                                    "rest": {"rest5xx": 1, "restTotal": 200}, "rest_code": "scraped"})

    def test_superseded_staging_skips_automatic_runs_and_refuses_manual_ones(self):
        skip, _ = gate.staging_step(probe(revision=NEWER), rest(0, 50), SHA, auto=True)
        self.assertEqual(skip, "staging-superseded")
        with self.assertRaises(gate.GateError):
            gate.staging_step(probe(revision=NEWER), rest(0, 50), SHA, auto=False)

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


def staged(checks=None, counters=(0, 200)):
    return {"sha": SHA, "checks": checks or {"health": "pass", "readyz": "pass", "build": "pass"},
            "rest": {"rest5xx": counters[0], "restTotal": counters[1]} if counters else None}


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

    def staging_args(self, revision=SHA, auto="true"):
        return ["staging", "--probe", self.write("probe.json", probe(revision=revision)),
                "--counters", self.write("counters.json", rest(0, 100)), "--sha", SHA,
                "--auto", auto, "--out", str(self.dir / "staging.json")]

    def test_staging_step_writes_outputs_for_the_production_job(self):
        code, _ = self.run_main(self.staging_args())
        self.assertEqual(code, 0)
        outputs = dict(line.split("=", 1) for line in self.output.read_text().splitlines())
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(json.loads(outputs["evidence"])["sha"], SHA)

    def test_staging_step_skips_or_refuses_a_superseded_candidate(self):
        code, _ = self.run_main(self.staging_args(revision=NEWER))
        self.assertEqual(code, 0)
        self.assertIn("skip=staging-superseded", self.output.read_text())
        self.assertIn("staging-superseded", self.summary.read_text())
        code, stdout = self.run_main(self.staging_args(revision=NEWER, auto="false"))
        self.assertEqual(code, 1)
        self.assertIn("::error::Refusing promotion", stdout)

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

import copy
import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

import compare_pipeline_bench as bench
import pipeline_bench_profile as profile


def report(sqlx=11417, events=900, server=None, per_event=None):
    """A real-shaped qualified report: 600 messages, 300 voice updates."""
    calls = sqlx + events
    server = server or {"-4611686018427387904": calls - 1000, "77": 600, "9223372036854775807": 400}
    per_event = per_event or {"MESSAGE_CREATE": [600, calls - 1500], "VOICE_STATE_UPDATE": [300, 1500]}
    return {
        "schema_version": 1,
        "workload": {"members": 107, "channels": 10, "profile": "debug"},
        "metrics": {
            "peak_rss_mib": 24.6,
            "handler_latency_us": {"p50": 3606.274, "p99": 91700.671},
            "db_round_trips_per_event": calls / events,
        },
        "evidence": {
            "measured_events": events, "sqlx_statements": sqlx,
            "begin_exchanges": events, "commits": events,
            "server_xact_commit": events, "server_xact_rollback": 0,
            "server_observer_transactions": 2, "server_statement_calls": calls,
            "server_statement_profile": server, "event_statement_profile": per_event,
            "expected_durable_events": 942, "durable_events": 942,
            "journal_rows": 942, "journaled_events": 942,
            "every_event_durable_after_drain": True, "disposable_database_dropped": True,
        },
    }


def improved():
    return report(
        sqlx=8717,
        server={"77": 600, "9223372036854775807": 400, "5": 8617},
        per_event={"MESSAGE_CREATE": [600, 6617], "VOICE_STATE_UPDATE": [300, 3000]},
    )


def run_cli(*texts, raw=None):
    with tempfile.TemporaryDirectory() as tmp:
        paths = []
        for index, text in enumerate(texts):
            path = Path(tmp) / f"r{index}.json"
            path.write_bytes(text if isinstance(text, bytes) else text.encode())
            paths.append(str(path))
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = profile.main(paths)
        return code, json.loads(out.getvalue()), err.getvalue()


class ProfileDiffTest(unittest.TestCase):
    def invalid(self, before, after, message=None):
        with self.assertRaises(ValueError) as caught:
            profile.diff(before, after)
        if message:
            self.assertIn(message, str(caught.exception))

    def test_valid_real_shaped_comparison(self):
        before = report()
        after = report(
            sqlx=8717,
            server={"-4611686018427387904": 7300, "77": 600, "9223372036854775807": 400, "5": 1317},
            per_event={"MESSAGE_CREATE": [600, 6617], "VOICE_STATE_UPDATE": [300, 3000]},
        )
        result = profile.diff(before, after)
        self.assertEqual(result["schema_version"], 1)
        self.assertEqual(result["measured_events"], 900)
        self.assertEqual(result["total_exchange_delta"], -2700)
        self.assertAlmostEqual(result["db_round_trips_per_event"]["delta"], -3.0)
        by_fp = {c["fingerprint"]: c for c in result["changed_server_fingerprints"]}
        self.assertEqual(set(by_fp), {"-4611686018427387904", "5"})
        self.assertEqual(by_fp["5"], {"fingerprint": "5", "before": 0, "after": 1317, "delta": 1317})
        self.assertEqual(sum(c["delta"] for c in by_fp.values()), -2700)
        voice = next(e for e in result["event_statement_profile"] if e["event"] == "VOICE_STATE_UPDATE")
        self.assertEqual(voice["before"], {"events": 300, "exchanges": 1500})
        self.assertEqual(voice["delta"], {"events": 0, "exchanges": 1500})
        message = next(e for e in result["event_statement_profile"] if e["event"] == "MESSAGE_CREATE")
        self.assertEqual(message["delta"], {"events": 0, "exchanges": -4200})
        self.assertNotIn("sql", json.dumps(result).lower())

    def test_identical_reports_have_empty_changes(self):
        result = profile.diff(report(), report())
        self.assertEqual(result["changed_server_fingerprints"], [])
        self.assertEqual(result["total_exchange_delta"], 0)

    def test_undercounting_is_invalid(self):
        bad = report()
        bad["evidence"]["sqlx_statements"] -= 1
        self.invalid(report(), bad)
        bad = report()
        bad["evidence"]["server_statement_calls"] -= 1
        self.invalid(report(), bad, "counter identity")
        bad = report()
        bad["evidence"]["begin_exchanges"] -= 1
        self.invalid(report(), bad, "counter identity")
        bad = report()
        bad["evidence"]["server_xact_commit"] -= 1
        self.invalid(report(), bad, "counter identity")

    def test_lost_durability_is_invalid(self):
        for name in ("durable_events", "journal_rows", "journaled_events"):
            bad = report()
            bad["evidence"][name] -= 1
            self.invalid(report(), bad)
        for name in ("every_event_durable_after_drain", "disposable_database_dropped"):
            for value in (False, 1, None):
                bad = report()
                bad["evidence"][name] = value
                self.invalid(report(), bad, "must be true")
        bad = report()
        bad["evidence"]["server_xact_rollback"] = 1
        self.invalid(report(), bad)
        bad = report()
        bad["evidence"]["server_observer_transactions"] = 0
        self.invalid(report(), bad)

    def test_bad_counts_are_invalid(self):
        for name, value in (("measured_events", True), ("measured_events", 0),
                            ("sqlx_statements", -1), ("sqlx_statements", 1.0),
                            ("sqlx_statements", "5"), ("expected_durable_events", 0),
                            ("commits", None)):
            bad = report()
            bad["evidence"][name] = value
            self.invalid(report(), bad)
        bad = report()
        del bad["evidence"]["sqlx_statements"]
        self.invalid(report(), bad)

    def test_empty_forged_or_inconsistent_profiles_are_invalid(self):
        for name in ("server_statement_profile", "event_statement_profile"):
            for value in ({}, [], None, "x"):
                bad = report()
                bad["evidence"][name] = value
                self.invalid(report(), bad, "non-empty object")
            bad = report()
            del bad["evidence"][name]
            self.invalid(report(), bad)
        sums = report()
        sums["evidence"]["server_statement_profile"]["77"] += 1
        self.invalid(report(), sums, "does not sum")
        for key in ("abc", "1.5", "", "9223372036854775808", "-9223372036854775809", " 7", "0x1"):
            bad = report()
            bad["evidence"]["server_statement_profile"][key] = bad["evidence"]["server_statement_profile"].pop("77")
            self.invalid(report(), bad, "fingerprint")
        for count in (True, 0, -1, 1.5, "600"):
            bad = report()
            bad["evidence"]["server_statement_profile"]["77"] = count
            self.invalid(report(), bad, "positive integers")
        for entry in ([600], [600, 1, 2], [True, 1], [0, 5], [600, 0], "ab", [600.0, 5]):
            bad = report()
            bad["evidence"]["event_statement_profile"]["MESSAGE_CREATE"] = entry
            self.invalid(report(), bad)
        bad = report()
        bad["evidence"]["event_statement_profile"]["VOICE_STATE_UPDATE"][0] += 1
        self.invalid(report(), bad, "event counts")
        bad = report()
        bad["evidence"]["event_statement_profile"]["VOICE_STATE_UPDATE"][1] += 1
        self.invalid(report(), bad, "exchanges")
        bad = report()
        bad["evidence"]["event_statement_profile"]["bad name"] = bad["evidence"]["event_statement_profile"].pop("MESSAGE_CREATE")
        self.invalid(report(), bad, "event name")

    def test_metric_must_match_counters(self):
        bad = report()
        bad["metrics"]["db_round_trips_per_event"] -= 0.5
        self.invalid(report(), bad, "does not match statement counters")
        bad = report()
        bad["metrics"]["db_round_trips_per_event"] = True
        self.invalid(report(), bad)

    def test_unlike_workloads_are_invalid(self):
        other = report()
        other["workload"]["members"] = 108
        self.invalid(report(), other, "workload differs")
        other = report(events=450, sqlx=5000)
        other["evidence"]["event_statement_profile"] = {"MESSAGE_CREATE": [450, 5450]}
        other["evidence"]["server_statement_profile"] = {"77": 5450}
        self.invalid(report(), other)

    def test_unqualified_historical_baseline_is_invalid(self):
        historical = json.loads(Path(__file__).resolve().parent.parent
                                .joinpath("docs/pipeline-benchmark-baseline.json").read_text())
        self.invalid(historical, report(), "unqualified")
        self.invalid(report(), historical)
        bare = report()
        del bare["evidence"]
        self.invalid(report(), bare, "evidence missing")
        self.invalid([], report())

    def test_inputs_are_not_mutated(self):
        before, after = report(), improved()
        snapshot = copy.deepcopy((before, after))
        profile.diff(before, after)
        self.assertEqual((before, after), snapshot)


class ProfileCliTest(unittest.TestCase):
    def assertRefused(self, code, *texts):
        status, out, err = run_cli(*texts)
        self.assertEqual(status, 2)
        self.assertEqual(out["verdict"], "INVALID")
        self.assertEqual(out["code"], code)
        self.assertTrue(err.startswith("INVALID benchmark:"))
        return err

    def test_cli_emits_json_diff(self):
        after = improved()
        status, out, err = run_cli(json.dumps(report()), json.dumps(after))
        self.assertEqual((status, err), (0, ""))
        self.assertEqual(out["total_exchange_delta"], -2700)

    def test_cli_refusals(self):
        good = json.dumps(report())
        self.assertRefused("invalid_json", good, "{not json")
        self.assertRefused("invalid_json", good, b"\xff\xfe")
        self.assertRefused("duplicate_keys", good, '{"a": 1, "a": 2}')
        dup = good.replace('"77"', '"77": 1, "77"', 1)
        self.assertRefused("duplicate_keys", dup, good)
        self.assertRefused("input_too_large", good, " " * (bench.MAX_INPUT_BYTES + 1))
        self.assertRefused("line_too_long", good, "[" + "1," * 40000 + "1]")
        self.assertRefused("too_many_records", good, "\n" * (bench.MAX_RECORDS + 1))
        err = self.assertRefused("invalid_benchmark", good, "[]")
        self.assertNotIn("[]", err)

    def test_cli_missing_file(self):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            status = profile.main(["/nonexistent/a.json", "/nonexistent/b.json"])
        self.assertEqual(status, 2)
        self.assertEqual(json.loads(out.getvalue())["code"], "unreadable_input")


if __name__ == "__main__":
    unittest.main()

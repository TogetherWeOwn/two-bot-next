"""Offline DORA scoring fixtures; no network, no gh (writes only land in temp dirs)."""

import json
import os
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dora_metrics


def record(deployment_id, state, status_at, **changes):
    base = {"id": deployment_id, "sha": "a" * 39 + str(deployment_id % 10),
            "deploy_id": f"2026.10.20.1+aaaaaa{deployment_id % 10}",
            "created_at": "2026-10-20T07:00:00Z", "state": state,
            "status_at": status_at, "log_url": "", "run_id": None,
            "run_conclusion": None, "run_head_sha": None, "run_verified": False,
            "commit_at": None, "lead_minutes": None, "commits_since_prev": None}
    base.update(changes)
    return base


def window(label="2026-W43"):
    return {"label": label, "days": 30, "start": "2026-09-22T07:17:00Z",
            "end": "2026-10-22T07:17:00Z"}


def generator():
    return {"workflow": "dora-metrics", "run_id": "4242", "sha": "b" * 40}


class MomentTests(unittest.TestCase):
    def test_zulu_and_offset_spellings_normalize(self):
        for raw in ("2026-10-22T07:17:00Z", "2026-10-22T07:17:00+00:00",
                    "2026-10-22T09:17:00+02:00"):
            moment = dora_metrics.parse_moment(raw)
            self.assertEqual(moment, datetime(2026, 10, 22, 7, 17, tzinfo=timezone.utc))
            self.assertEqual(dora_metrics.format_moment(moment), "2026-10-22T07:17:00Z")

    def test_unzoned_missing_and_garbage_refuse(self):
        for raw in ("2026-10-22T07:17:00", "", None, "not a time", 1720000000):
            with self.subTest(raw=raw):
                with self.assertRaises(dora_metrics.DoraError):
                    dora_metrics.parse_moment(raw)

    def test_week_label_follows_iso_year_boundaries(self):
        cases = {datetime(2026, 1, 1, tzinfo=timezone.utc): "2026-W01",
                 datetime(2026, 10, 22, 7, 17, tzinfo=timezone.utc): "2026-W43",
                 datetime(2027, 1, 1, tzinfo=timezone.utc): "2026-W53",
                 datetime(2027, 1, 4, tzinfo=timezone.utc): "2027-W01"}
        for moment, label in cases.items():
            with self.subTest(moment=moment.isoformat()):
                self.assertEqual(dora_metrics.week_label(moment), label)


class ParseRunIdTests(unittest.TestCase):
    def test_run_id_parses_from_log_urls(self):
        url = "https://github.com/o/r/actions/runs/42424242"
        self.assertEqual(dora_metrics.parse_run_id(url), 42424242)
        self.assertEqual(dora_metrics.parse_run_id(url + "/job/1"), 42424242)

    def test_missing_or_foreign_urls_yield_no_run(self):
        for url in ("", None, "https://example.com/runs/123", "not a url"):
            with self.subTest(url=url):
                self.assertIsNone(dora_metrics.parse_run_id(url))


class PercentileTests(unittest.TestCase):
    def test_nearest_rank_picks_documented_members(self):
        values = [10.0, 20.0, 30.0, 40.0]
        self.assertEqual(dora_metrics.percentile_nearest_rank(values, 50), 20.0)
        self.assertEqual(dora_metrics.percentile_nearest_rank(values, 90), 40.0)
        self.assertEqual(dora_metrics.percentile_nearest_rank([7.5], 90), 7.5)


class ChangeFailureRateTests(unittest.TestCase):
    def test_failed_over_resolved_with_pass_line(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z"),
                   record(2, "failure", "2026-10-21T07:10:00Z")]
        report = dora_metrics.build_report("o/r", records, 3, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        g2 = report["g2_change_failure_rate"]
        self.assertEqual((g2["failed"], g2["total"], g2["rate"]), (1, 2, 0.5))
        self.assertFalse(g2["pass"])
        self.assertEqual(g2["excluded_automatic_rows"], 3)

    def test_unknown_and_in_flight_never_enter_the_rate(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z"),
                   record(2, "unknown", None),
                   record(3, "in_progress", "2026-10-22T07:10:00Z")]
        report = dora_metrics.build_report("o/r", records, 0, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        g2 = report["g2_change_failure_rate"]
        self.assertEqual((g2["failed"], g2["total"], g2["rate"]), (0, 1, 0.0))
        self.assertTrue(g2["pass"])
        self.assertEqual((g2["unknown_state"], g2["in_flight"]), (1, 1))

    def test_empty_window_reads_null_never_zero(self):
        report = dora_metrics.build_report("o/r", [], 0, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        g2 = report["g2_change_failure_rate"]
        self.assertEqual((g2["failed"], g2["total"]), (0, 0))
        self.assertIsNone(g2["rate"])
        self.assertIsNone(g2["pass"])


class RecoveryTests(unittest.TestCase):
    def test_failed_pairs_with_the_next_green(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z"),
                   record(2, "failure", "2026-10-20T08:00:00Z"),
                   record(3, "success", "2026-10-20T08:04:00Z")]
        with patch.object(dora_metrics, "fetch_is_rollback", return_value=False):
            incidents, unrecovered, unverified = dora_metrics.score_recovery("o/r", records)
        self.assertEqual(unrecovered, [])
        self.assertEqual(unverified, 0)
        [incident] = incidents
        self.assertEqual(incident["failed_deployment_id"], 2)
        self.assertEqual(incident["restored_by_deployment_id"], 3)
        self.assertEqual(incident["recovery_minutes"], 4.0)
        self.assertEqual(incident["mechanism"], "next_green")

    def test_rollback_mechanism_counts_toward_the_auto_target(self):
        incidents = [{"failed_deployment_id": 2, "failed_at": "2026-10-20T08:00:00Z",
                      "restored_by_deployment_id": 3, "restored_at": "2026-10-20T08:03:00Z",
                      "recovery_minutes": 3.0, "mechanism": "rollback"}]
        report = dora_metrics.build_report("o/r", [], 0, window(),
                                           (incidents, [], 0), 0,
                                           "2026-10-22T07:17:01Z", generator())
        g3 = report["g3_recovery_minutes"]
        self.assertEqual((g3["worst_minutes"], g3["mean_minutes"]), (3.0, 3.0))
        self.assertTrue(g3["worst_pass"])
        self.assertEqual(g3["rollback_recoveries"], 1)
        self.assertTrue(g3["rollback_all_under_5"])

    def test_unrecovered_failures_list_without_a_verdict(self):
        unrecovered = [{"deployment_id": 9, "sha": "c" * 40,
                        "failed_at": "2026-10-22T07:00:00Z"}]
        report = dora_metrics.build_report("o/r", [], 0, window(),
                                           ([], unrecovered, 0), 0,
                                           "2026-10-22T07:17:01Z", generator())
        g3 = report["g3_recovery_minutes"]
        self.assertEqual(g3["unrecovered"], unrecovered)
        self.assertIsNone(g3["worst_minutes"])
        self.assertIsNone(g3["worst_pass"])

    def test_unreadable_payload_marks_mechanism_unverified(self):
        records = [record(2, "failure", "2026-10-20T08:00:00Z"),
                   record(3, "success", "2026-10-20T08:04:00Z")]
        with patch.object(dora_metrics, "fetch_is_rollback", return_value=None):
            incidents, _unrecovered, unverified = dora_metrics.score_recovery("o/r", records)
        self.assertEqual(unverified, 1)
        self.assertEqual(incidents[0]["mechanism"], "unverified")


class LeadTimeTests(unittest.TestCase):
    def test_percentiles_and_pass_lines(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z", lead_minutes=10.0),
                   record(2, "success", "2026-10-21T07:10:00Z", lead_minutes=20.0),
                   record(3, "success", "2026-10-22T07:10:00Z", lead_minutes=30.0),
                   record(4, "success", "2026-10-22T07:20:00Z", lead_minutes=40.0)]
        report = dora_metrics.build_report("o/r", records, 0, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        g4 = report["g4_lead_time_minutes"]
        self.assertEqual((g4["p50_minutes"], g4["p90_minutes"], g4["n"]), (20.0, 40.0, 4))
        self.assertEqual(g4["method"], "nearest-rank")
        self.assertTrue(g4["p50_pass"])
        self.assertTrue(g4["p90_pass"])
        self.assertEqual(g4["unresolved_commits"], 0)

    def test_unresolved_commits_count_without_skewing_percentiles(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z", lead_minutes=10.0),
                   record(2, "success", "2026-10-21T07:10:00Z")]
        report = dora_metrics.build_report("o/r", records, 0, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        g4 = report["g4_lead_time_minutes"]
        self.assertEqual(g4["n"], 1)
        self.assertEqual(g4["unresolved_commits"], 1)


class ThroughputTests(unittest.TestCase):
    def test_per_day_and_commits_cap(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z", commits_since_prev=3),
                   record(2, "success", "2026-10-20T08:10:00Z", commits_since_prev=9),
                   record(3, "failure", "2026-10-21T07:10:00Z")]
        report = dora_metrics.build_report("o/r", records, 0, window(),
                                           ([], [], 0), 1, "2026-10-22T07:17:01Z",
                                           generator())
        g5 = report["g5_throughput"]
        self.assertEqual(g5["successful_deploys"], 2)
        self.assertEqual(g5["per_day"], {"2026-10-20": 2})
        self.assertEqual(g5["mean_per_day"], round(2 / 30, 2))
        cpd = g5["commits_per_deploy"]
        self.assertEqual((cpd["mean"], cpd["max"]), (6.0, 9))
        self.assertTrue(cpd["pass"])
        self.assertEqual(cpd["unresolved"], 1)

    def test_oversized_deploy_fails_the_cap(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z", commits_since_prev=16)]
        report = dora_metrics.build_report("o/r", records, 0, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        self.assertFalse(report["g5_throughput"]["commits_per_deploy"]["pass"])

    def test_compare_reads_chain_successive_shas(self):
        first = record(1, "success", "2026-10-20T07:10:00Z")
        first["sha"] = "d" * 40
        second = record(2, "success", "2026-10-21T07:10:00Z")
        second["sha"] = "e" * 40
        with patch.object(dora_metrics, "fetch_ahead_by", return_value=4) as ahead:
            unresolved = dora_metrics.score_throughput("o/r", [first, second], "f" * 40)
        self.assertEqual(unresolved, 0)
        self.assertEqual([first["commits_since_prev"], second["commits_since_prev"]], [4, 4])
        self.assertEqual([call.args[1:] for call in ahead.call_args_list],
                         [("f" * 40, "d" * 40), ("d" * 40, "e" * 40)])

    def test_missing_predecessor_counts_unresolved_once(self):
        first = record(1, "success", "2026-10-20T07:10:00Z")
        with patch.object(dora_metrics, "fetch_ahead_by") as ahead:
            unresolved = dora_metrics.score_throughput("o/r", [first], None)
        self.assertEqual(unresolved, 1)
        self.assertIsNone(first["commits_since_prev"])
        ahead.assert_not_called()


class ResolveTests(unittest.TestCase):
    def test_records_sort_oldest_first_with_run_enrichment(self):
        rows = [
            {"id": 2, "sha": "a" * 40, "description": "2026.10.21.1+" + "a" * 7,
             "created_at": "2026-10-21T07:00:00Z"},
            {"id": 1, "sha": "b" * 40, "description": "2026.10.20.1+" + "b" * 7,
             "created_at": "2026-10-20T07:00:00Z"},
        ]
        statuses = {1: ("success", datetime(2026, 10, 20, 7, 10, tzinfo=timezone.utc),
                        "https://github.com/o/r/actions/runs/111"),
                    2: ("failure", datetime(2026, 10, 21, 7, 10, tzinfo=timezone.utc), "")}
        with patch.object(dora_metrics, "latest_status",
                          side_effect=lambda _repo, dep_id: statuses[dep_id]), \
             patch.object(dora_metrics, "fetch_run", return_value=("success", "b" * 40)), \
             patch.object(dora_metrics, "fetch_commit_at",
                          return_value=datetime(2026, 10, 20, 6, 50, tzinfo=timezone.utc)):
            records = dora_metrics.resolve_deployments("o/r", rows)
        self.assertEqual([r["id"] for r in records], [1, 2])
        self.assertEqual(records[0]["run_id"], 111)
        self.assertTrue(records[0]["run_verified"])
        self.assertEqual(records[0]["lead_minutes"], 20.0)
        self.assertIsNone(records[1]["run_id"])
        self.assertIsNone(records[1]["lead_minutes"])

    def test_statusless_row_lands_in_the_unknown_bucket(self):
        rows = [{"id": 1, "sha": "a" * 40, "description": "2026.10.20.1+" + "a" * 7,
                 "created_at": "2026-10-20T07:00:00Z"}]
        with patch.object(dora_metrics, "latest_status", return_value=None):
            [found] = dora_metrics.resolve_deployments("o/r", rows)
        self.assertEqual(found["state"], "unknown")
        self.assertIsNone(found["status_at"])


class RenderTests(unittest.TestCase):
    def test_summary_names_every_verdict_and_note(self):
        records = [record(1, "success", "2026-10-20T07:10:00Z",
                          lead_minutes=10.0, commits_since_prev=3),
                   record(2, "unknown", None)]
        report = dora_metrics.build_report(
            "o/r", records, 0, window(),
            ([], [{"deployment_id": 9, "sha": "c" * 40,
                   "failed_at": "2026-10-22T07:00:00Z"}], 0),
            0, "2026-10-22T07:17:01Z", generator())
        text = dora_metrics.render_markdown(report)
        self.assertIn("# DORA metrics 2026-W43", text)
        self.assertIn("G2 change failure rate", text)
        self.assertIn("deployment 9", text)
        self.assertIn("carry no status yet", text)
        self.assertIn("`dora-2026-W43.json`", text)
        # G5 deploys/day has no target, so no verdict, but the computed
        # value still renders: one success over 30 days reads 0.03, not n/a.
        self.assertIn("| G5 deploys/day | 0.03 | >=1/hour when green work exists | n/a |",
                      text)

    def test_empty_week_renders_n_a_without_a_false_pass(self):
        report = dora_metrics.build_report("o/r", [], 0, window(),
                                           ([], [], 0), 0, "2026-10-22T07:17:01Z",
                                           generator())
        text = dora_metrics.render_markdown(report)
        self.assertIn("n/a", text)
        self.assertNotIn("FAIL", text)
        self.assertNotIn("| pass |", text)


class CliTests(unittest.TestCase):
    def test_bad_flags_and_missing_repo_fail_before_any_read(self):
        with patch.object(dora_metrics, "gh_api_get") as api:
            with patch.dict("os.environ", {"GITHUB_REPOSITORY": "o/r"}, clear=True):
                self.assertEqual(dora_metrics.main(["--week", "next-week"]), 1)
                self.assertEqual(dora_metrics.main(["--window-days", "0"]), 1)
                self.assertEqual(dora_metrics.main(["--now", "not a time"]), 1)
            with patch.dict("os.environ", {}, clear=True):
                self.assertEqual(dora_metrics.main([]), 1)
            api.assert_not_called()

    def test_main_writes_json_and_markdown_with_week_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "metrics")
            output_file = os.path.join(tmp, "github_output")
            records = [record(1, "success", "2026-10-20T07:10:00Z",
                              lead_minutes=10.0, commits_since_prev=3)]
            with patch.object(dora_metrics, "list_production_deployments",
                              return_value=([{"id": 1}], 2, None)), \
                 patch.object(dora_metrics, "resolve_deployments",
                              return_value=records), \
                 patch.object(dora_metrics, "score_throughput", return_value=0), \
                 patch.object(dora_metrics, "score_recovery",
                              return_value=([], [], 0)), \
                 patch.dict("os.environ", {"GITHUB_REPOSITORY": "o/r",
                                           "GITHUB_OUTPUT": output_file,
                                           "GITHUB_RUN_ID": "7",
                                           "GITHUB_SHA": "d" * 40}, clear=True):
                self.assertEqual(dora_metrics.main(
                    ["--out-dir", out, "--now", "2026-10-22T07:17:00Z"]), 0)
            report = json.loads(Path(out, "dora-2026-W43.json").read_text())
            self.assertEqual((report["schema_version"], report["week"]), (1, "2026-W43"))
            self.assertEqual(report["deployments"], records)
            self.assertEqual(report["g2_change_failure_rate"]["excluded_automatic_rows"], 2)
            summary = Path(out, "dora-2026-W43.md").read_text()
            self.assertIn("# DORA metrics 2026-W43", summary)
            self.assertIn("week=2026-W43", Path(output_file).read_text())


if __name__ == "__main__":
    unittest.main()

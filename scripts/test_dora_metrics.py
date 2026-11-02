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


def deployment(deployment_id, created_at="2026-10-20T07:00:00Z", **changes):
    row = {"id": deployment_id, "sha": "a" * 40,
           "description": f"2026.10.20.{deployment_id}+aaaaaaa", "created_at": created_at}
    row.update(changes)
    return row


def status_response(state, at="2026-10-20T07:10:00Z", **changes):
    row = {"state": state, "created_at": at, "log_url": ""}
    row.update(changes)
    return [row]


class DeploymentListingApiTests(unittest.TestCase):
    def test_only_canonical_ids_score_and_window_start_is_inclusive(self):
        start = dora_metrics.parse_moment(window()["start"])
        newest = deployment(9)
        at_start = deployment(2, window()["start"])
        boundary = deployment(1, "2026-09-22T07:16:59Z")
        automatic = [deployment(8, description=None), deployment(7, description=""),
                     deployment(6, description="Production deploy"),
                     deployment(5, description="2026.10.20.1+not-hex"),
                     deployment(4, description="prefix 2026.10.20.1+aaaaaaa")]
        with patch.object(dora_metrics, "gh_api_get",
                          return_value=[newest, *automatic, at_start, boundary]) as api:
            rows, excluded, predecessor = dora_metrics.list_production_deployments("o/r", start)
        self.assertEqual(rows, [newest, at_start])
        self.assertEqual(excluded, len(automatic))
        self.assertEqual(predecessor, boundary)
        api.assert_called_once_with("repos/o/r/deployments", environment="production",
                                    per_page=dora_metrics.DEPLOYMENTS_PER_PAGE, page=1)

    def test_old_automatic_rows_do_not_hide_canonical_predecessor_across_pages(self):
        newest = deployment(5)
        old_auto = deployment(4, "2026-09-22T07:16:59Z", description="")
        older_auto = deployment(3, "2026-09-22T07:16:58Z", description=None)
        boundary = deployment(2, "2026-09-22T07:16:57Z")
        with patch.object(dora_metrics, "DEPLOYMENTS_PER_PAGE", 2), \
             patch.object(dora_metrics, "gh_api_get",
                          side_effect=[[newest, old_auto], [older_auto, boundary]]) as api:
            rows, excluded, predecessor = dora_metrics.list_production_deployments(
                "o/r", dora_metrics.parse_moment(window()["start"]))
        self.assertEqual(rows, [newest])
        self.assertEqual(excluded, 0)
        self.assertEqual(predecessor, boundary)
        self.assertEqual([c.kwargs for c in api.call_args_list],
                         [{"environment": "production", "per_page": 2, "page": 1},
                          {"environment": "production", "per_page": 2, "page": 2}])

    def test_stops_at_first_canonical_pre_window_row(self):
        newest = deployment(3)
        boundary = deployment(2, "2026-09-22T07:16:59Z")
        older = deployment(1, "2026-09-21T07:00:00Z")
        with patch.object(dora_metrics, "DEPLOYMENTS_PER_PAGE", 2), \
             patch.object(dora_metrics, "gh_api_get",
                          side_effect=[[newest, boundary], [older]]) as api:
            rows, excluded, predecessor = dora_metrics.list_production_deployments(
                "o/r", dora_metrics.parse_moment(window()["start"]))
        self.assertEqual((rows, excluded, predecessor), ([newest], 0, boundary))
        api.assert_called_once_with("repos/o/r/deployments", environment="production",
                                    per_page=2, page=1)

    def test_pagination_cap_fails_closed_instead_of_returning_partial_metrics(self):
        with patch.object(dora_metrics, "DEPLOYMENTS_PER_PAGE", 2), \
             patch.object(dora_metrics, "DEPLOYMENTS_MAX_PAGES", 2), \
             patch.object(dora_metrics, "gh_api_get",
                          return_value=[deployment(2), deployment(1)]) as api:
            with self.assertRaises(dora_metrics.DoraError):
                dora_metrics.list_production_deployments(
                    "o/r", dora_metrics.parse_moment(window()["start"]))
        self.assertEqual([c.kwargs["page"] for c in api.call_args_list], [1, 2])

    def test_malformed_list_rows_and_timestamps_fail_closed(self):
        for response in (None, {}, [None], ["row"], [deployment(1, None)],
                         [deployment(1, "not a timestamp")],
                         [deployment(1, "2026-10-20T07:00:00")]):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                with self.assertRaises(dora_metrics.DoraError):
                    dora_metrics.list_production_deployments(
                        "o/r", dora_metrics.parse_moment(window()["start"]))


class StatusApiTests(unittest.TestCase):
    def test_empty_status_list_is_unknown(self):
        with patch.object(dora_metrics, "gh_api_get", return_value=[]) as api:
            self.assertIsNone(dora_metrics.latest_status("o/r", 17))
        api.assert_called_once_with("repos/o/r/deployments/17/statuses", per_page=1)

    def test_success_failure_and_in_flight_states_are_not_rewritten(self):
        for state in ("success", "failure", "error", "pending", "in_progress", "queued"):
            with self.subTest(state=state), patch.object(
                    dora_metrics, "gh_api_get", return_value=status_response(
                        state, "2026-10-20T09:10:00+02:00",
                        log_url="https://github.com/o/r/actions/runs/111")):
                self.assertEqual(dora_metrics.latest_status("o/r", 17),
                                 (state, datetime(2026, 10, 20, 7, 10, tzinfo=timezone.utc),
                                  "https://github.com/o/r/actions/runs/111"))

    def test_non_string_log_url_is_not_a_run_link(self):
        with patch.object(dora_metrics, "gh_api_get",
                          return_value=status_response("success", log_url=123)):
            self.assertEqual(dora_metrics.latest_status("o/r", 17)[2], "")

    def test_malformed_status_list_state_and_timestamp_raise_dora_error(self):
        for response in (None, {}, "statuses", [{}], status_response(None),
                         status_response(""), status_response(1),
                         status_response("success", None),
                         status_response("success", "not a timestamp"),
                         status_response("success", "2026-10-20T07:10:00")):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                with self.assertRaises(dora_metrics.DoraError):
                    dora_metrics.latest_status("o/r", 17)

    def test_non_dict_first_status_row_is_malformed_not_unknown(self):
        for first in (None, "success", 1, []):
            with self.subTest(first=first), \
                 patch.object(dora_metrics, "gh_api_get", return_value=[first]):
                with self.assertRaises(dora_metrics.DoraError):
                    dora_metrics.latest_status("o/r", 17)

    def test_status_read_failure_does_not_turn_into_unknown(self):
        with patch.object(dora_metrics, "gh_api_get",
                          side_effect=dora_metrics.DoraError("github api refused the read")):
            with self.assertRaises(dora_metrics.DoraError):
                dora_metrics.latest_status("o/r", 17)


class EnrichmentApiTests(unittest.TestCase):
    def test_rollback_reads_dict_and_json_string_booleans_honestly(self):
        for rollback in (False, True):
            for payload in ({"rollback": rollback}, json.dumps({"rollback": rollback})):
                with self.subTest(payload=payload), patch.object(
                        dora_metrics, "gh_api_get", return_value={"payload": payload}) as api:
                    self.assertIs(dora_metrics.fetch_is_rollback("o/r", 17), rollback)
                api.assert_called_once_with("repos/o/r/deployments/17")

    def test_missing_rollback_payload_is_false_not_a_rollback(self):
        for response in ({}, {"payload": None}, {"payload": {}},
                         {"payload": {"rollback": "true"}}, {"payload": {"rollback": 1}}):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                self.assertIs(dora_metrics.fetch_is_rollback("o/r", 17), False)

    def test_unreadable_rollback_shape_or_payload_is_unverified(self):
        for response in (None, [], "deployment", {"payload": "garbage"},
                         {"payload": "[]"}, {"payload": "true"},
                         {"payload": "null"}, {"payload": []}, {"payload": 1}):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                self.assertIsNone(dora_metrics.fetch_is_rollback("o/r", 17))

    def test_compare_accepts_only_nonnegative_integer_counts(self):
        for ahead in (0, 4, 16):
            with self.subTest(ahead=ahead), patch.object(
                    dora_metrics, "gh_api_get", return_value={"ahead_by": ahead}) as api:
                self.assertEqual(dora_metrics.fetch_ahead_by("o/r", "b" * 40, "a" * 40), ahead)
            api.assert_called_once_with("repos/o/r/compare/" + "b" * 40 + "..." + "a" * 40)
        for response in (None, [], 4, {}, {"ahead_by": None}, {"ahead_by": -1},
                         {"ahead_by": True}, {"ahead_by": False},
                         {"ahead_by": "4"}, {"ahead_by": 4.0}, {"ahead_by": 3.5},
                         {"ahead_by": []}, {"ahead_by": {"count": 4}}):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                self.assertIsNone(dora_metrics.fetch_ahead_by("o/r", "base", "head"))

    def test_run_preserves_conclusion_and_head_sha(self):
        for conclusion in ("success", "failure", "cancelled"):
            with self.subTest(conclusion=conclusion), patch.object(
                    dora_metrics, "gh_api_get",
                    return_value={"conclusion": conclusion, "head_sha": "b" * 40}) as api:
                self.assertEqual(dora_metrics.fetch_run("o/r", 111), (conclusion, "b" * 40))
            api.assert_called_once_with("repos/o/r/actions/runs/111")

    def test_malformed_or_incomplete_run_is_unverified(self):
        for response in (None, [], "run", {}, {"conclusion": "success"},
                         {"conclusion": None, "head_sha": "b" * 40},
                         {"conclusion": 1, "head_sha": "b" * 40},
                         {"conclusion": "success", "head_sha": None},
                         {"conclusion": "success", "head_sha": 123}):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                self.assertIsNone(dora_metrics.fetch_run("o/r", 111))

    def test_commit_date_reads_nested_committer_and_normalizes_timezone(self):
        response = {"commit": {"committer": {"date": "2026-10-20T08:50:00+02:00"}}}
        with patch.object(dora_metrics, "gh_api_get", return_value=response) as api:
            self.assertEqual(dora_metrics.fetch_commit_at("o/r", "a" * 40),
                             datetime(2026, 10, 20, 6, 50, tzinfo=timezone.utc))
        api.assert_called_once_with("repos/o/r/commits/" + "a" * 40)

    def test_malformed_commit_nesting_or_date_is_unresolved(self):
        for response in (None, [], "commit", {}, {"commit": []}, {"commit": {}},
                         {"commit": {"committer": []}}, {"commit": {"committer": {}}},
                         {"commit": {"committer": {"date": None}}},
                         {"commit": {"committer": {"date": "garbage"}}},
                         {"commit": {"committer": {"date": "2026-10-20T06:50:00"}}}):
            with self.subTest(response=response), \
                 patch.object(dora_metrics, "gh_api_get", return_value=response):
                self.assertIsNone(dora_metrics.fetch_commit_at("o/r", "a" * 40))

    def test_enrichment_read_failures_remain_null(self):
        helpers = [(dora_metrics.fetch_is_rollback, ("o/r", 17)),
                   (dora_metrics.fetch_ahead_by, ("o/r", "base", "head")),
                   (dora_metrics.fetch_run, ("o/r", 111)),
                   (dora_metrics.fetch_commit_at, ("o/r", "a" * 40))]
        for helper, args in helpers:
            with self.subTest(helper=helper.__name__), patch.object(
                    dora_metrics, "gh_api_get",
                    side_effect=dora_metrics.DoraError("github api refused the read")):
                self.assertIsNone(helper(*args))


class ResolutionApiTests(unittest.TestCase):
    def test_run_url_without_a_readable_run_never_sets_run_verified(self):
        responses = {
            "repos/o/r/deployments/1/statuses": status_response(
                "success", log_url="https://github.com/o/r/actions/runs/111"),
            "repos/o/r/commits/" + "a" * 40:
                {"commit": {"committer": {"date": "2026-10-20T06:50:00Z"}}},
        }

        def read(path, **fields):
            if path == "repos/o/r/actions/runs/111":
                raise dora_metrics.DoraError("github api refused the read")
            return responses[path]

        with patch.object(dora_metrics, "gh_api_get", side_effect=read) as api:
            [found] = dora_metrics.resolve_deployments("o/r", [deployment(1)])
        self.assertEqual(found["run_id"], 111)
        self.assertFalse(found["run_verified"])
        self.assertIsNone(found["run_conclusion"])
        self.assertIsNone(found["run_head_sha"])
        self.assertEqual(found["lead_minutes"], 20.0)
        self.assertIn("repos/o/r/actions/runs/111", [c.args[0] for c in api.call_args_list])

    def test_g2_counts_failure_unknown_and_in_flight_through_real_resolution(self):
        states = ["success", "failure", "error", None, "pending", "in_progress"]
        responses = {f"repos/o/r/deployments/{i}/statuses":
                     status_response(state) if state is not None else []
                     for i, state in enumerate(states, 1)}
        responses["repos/o/r/commits/" + "a" * 40] = {
            "commit": {"committer": {"date": "2026-10-20T06:50:00Z"}}}
        with patch.object(dora_metrics, "gh_api_get",
                          side_effect=lambda path, **fields: responses[path]):
            records = dora_metrics.resolve_deployments(
                "o/r", [deployment(i) for i in range(len(states), 0, -1)])
        self.assertEqual([r["state"] for r in records],
                         ["success", "failure", "error", "unknown", "pending", "in_progress"])
        self.assertEqual([r["id"] for r in records], list(range(1, 7)))
        self.assertIsNone(records[3]["status_at"])
        report = dora_metrics.build_report("o/r", records, 2, window(), ([], [], 0), 0,
                                           "2026-10-22T07:17:01Z", generator())
        g2 = report["g2_change_failure_rate"]
        self.assertEqual((g2["failed"], g2["total"], g2["rate"]), (2, 3, 0.6667))
        self.assertEqual((g2["unknown_state"], g2["in_flight"]), (1, 2))
        self.assertEqual(g2["excluded_automatic_rows"], 2)
        self.assertFalse(g2["pass"])


class BoundaryApiTests(unittest.TestCase):
    def run_boundary_report(self, boundary_state, boundary_sha="b" * 40):
        boundary = deployment(1, "2026-09-22T07:16:59Z", sha=boundary_sha)
        first = deployment(2)
        responses = {
            "repos/o/r/deployments": [first, boundary],
            "repos/o/r/deployments/2/statuses": status_response("success"),
            "repos/o/r/deployments/1/statuses": (
                [] if boundary_state is None else
                status_response(boundary_state, "2026-09-22T07:16:59Z")),
            "repos/o/r/commits/" + "a" * 40:
                {"commit": {"committer": {"date": "2026-10-20T06:50:00Z"}}},
        }
        compares = []

        def read(path, **fields):
            if path.startswith("repos/o/r/compare/"):
                compares.append(path)
                return {"ahead_by": 4}
            return responses[path]

        with tempfile.TemporaryDirectory() as out, \
             patch.object(dora_metrics, "gh_api_get", side_effect=read), \
             patch.dict("os.environ", {}, clear=True):
            self.assertEqual(dora_metrics.main([
                "--repo", "o/r", "--out-dir", out, "--now", window()["end"]]), 0)
            report = json.loads(Path(out, "dora-2026-W43.json").read_text())
        self.assertEqual([r["id"] for r in report["deployments"]], [2])
        return report, compares

    def assert_unresolved_boundary(self, state, sha="b" * 40):
        report, compares = self.run_boundary_report(state, sha)
        self.assertIsNone(report["deployments"][0]["commits_since_prev"])
        self.assertEqual(report["g5_throughput"]["commits_per_deploy"]["unresolved"], 1)
        self.assertEqual(compares, [])

    def test_failed_or_error_boundary_is_not_a_successful_predecessor(self):
        for state in ("failure", "error"):
            with self.subTest(state=state):
                self.assert_unresolved_boundary(state)

    def test_unknown_or_statusless_boundary_is_not_a_successful_predecessor(self):
        for state in ("unknown", None):
            with self.subTest(state=state):
                self.assert_unresolved_boundary(state)

    def test_pending_or_in_flight_boundary_is_not_a_successful_predecessor(self):
        for state in ("pending", "in_progress", "queued"):
            with self.subTest(state=state):
                self.assert_unresolved_boundary(state)

    def test_successful_boundary_requires_a_nonempty_string_sha(self):
        for sha in (None, "", 0, 42, [], {}):
            with self.subTest(sha=sha):
                self.assert_unresolved_boundary("success", sha)

    def test_successful_boundary_compares_the_expected_shas(self):
        report, compares = self.run_boundary_report("success")
        self.assertEqual(compares, ["repos/o/r/compare/" + "b" * 40 + "..." + "a" * 40])
        self.assertEqual(report["deployments"][0]["commits_since_prev"], 4)
        self.assertEqual(report["g5_throughput"]["commits_per_deploy"]["unresolved"], 0)


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

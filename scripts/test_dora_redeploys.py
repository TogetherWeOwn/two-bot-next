"""Independent throughput acceptance: a known same-SHA redeploy is zero, not null.

Exercise the real scorer and report builder. Only the GitHub read transport is
mocked; comparisons, predecessor selection and aggregation stay production code.
"""

from pathlib import Path
import sys
import unittest
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dora_metrics


REPO = "fixture/dora"
A, B, C = "a" * 40, "b" * 40, "c" * 40


def record(deployment_id, sha, state="success"):
    at = f"2026-10-20T07:{deployment_id:02d}:00Z"
    return {
        "id": deployment_id, "sha": sha,
        "deploy_id": f"2026.10.20.{deployment_id}+{sha[:7]}",
        "created_at": at, "state": state,
        "status_at": None if state == "unknown" else at,
        "log_url": "", "run_id": None, "run_conclusion": None,
        "run_head_sha": None, "run_verified": False,
        "commit_at": None, "lead_minutes": None, "commits_since_prev": None,
    }


def score_and_report(records, boundary):
    unresolved = dora_metrics.score_throughput(REPO, records, boundary)
    report = dora_metrics.build_report(
        REPO, records, 0,
        {"label": "2026-W43", "days": 30,
         "start": "2026-09-22T07:17:00Z", "end": "2026-10-22T07:17:00Z"},
        ([], [], 0), unresolved, "2026-10-22T07:17:01Z",
        {"workflow": "dora-metrics", "run_id": "4242", "sha": C},
    )
    return unresolved, report


class RedeployThroughputTests(unittest.TestCase):
    def assert_scored(self, records, unresolved, report, counts, mean, maximum,
                      passed, expected_unresolved):
        self.assertEqual(report["deployments"], records)
        self.assertEqual(report["g5_throughput"]["successful_deploys"],
                         sum(r["state"] == "success" for r in records))
        self.assertEqual(
            {"counts": [r["commits_since_prev"] for r in records],
             "unresolved": unresolved,
             "metric": report["g5_throughput"]["commits_per_deploy"]},
            {"counts": counts, "unresolved": expected_unresolved,
             "metric": {"mean": mean, "max": maximum, "target_max_le": 15,
                        "pass": passed, "unresolved": expected_unresolved}},
        )

    def test_boundary_a_then_b_then_b_counts_ten_and_zero_mean_five(self):
        records = [record(1, B), record(2, B)]
        with patch.object(dora_metrics, "gh_api_get", return_value={"ahead_by": 10}) as api:
            unresolved, report = score_and_report(records, A)
        api.assert_called_once_with(f"repos/{REPO}/compare/{A}...{B}")
        self.assert_scored(records, unresolved, report, [10, 0], 5.0, 10, True, 0)

    def test_success_equal_to_successful_boundary_is_known_zero_without_api(self):
        records = [record(1, A)]
        with patch.object(dora_metrics, "gh_api_get") as api:
            unresolved, report = score_and_report(records, A)
        api.assert_not_called()
        self.assert_scored(records, unresolved, report, [0], 0.0, 0, True, 0)

    def test_repeated_boundary_sha_successes_all_contribute_zero(self):
        records = [record(1, A), record(2, A)]
        with patch.object(dora_metrics, "gh_api_get") as api:
            unresolved, report = score_and_report(records, A)
        api.assert_not_called()
        self.assert_scored(records, unresolved, report, [0, 0], 0.0, 0, True, 0)

    def test_first_success_without_predecessor_remains_unknown(self):
        records = [record(1, A)]
        with patch.object(dora_metrics, "gh_api_get") as api:
            unresolved, report = score_and_report(records, None)
        api.assert_not_called()
        self.assert_scored(records, unresolved, report, [None], None, None, None, 1)

    def test_missing_predecessor_then_repeat_keeps_unknown_distinct_from_zero(self):
        records = [record(1, A), record(2, A)]
        with patch.object(dora_metrics, "gh_api_get") as api:
            unresolved, report = score_and_report(records, None)
        api.assert_not_called()
        self.assert_scored(records, unresolved, report, [None, 0], 0.0, 0, True, 1)

    def test_failed_compare_then_repeat_has_one_unresolved_and_one_known_zero(self):
        records = [record(1, B), record(2, B)]
        with patch.object(dora_metrics, "gh_api_get",
                          side_effect=dora_metrics.DoraError("github api refused the read")) as api:
            unresolved, report = score_and_report(records, A)
        api.assert_called_once_with(f"repos/{REPO}/compare/{A}...{B}")
        self.assert_scored(records, unresolved, report, [None, 0], 0.0, 0, True, 1)

    def test_non_success_rows_neither_advance_predecessor_nor_gain_counts(self):
        states = ("failure", "error", "in_progress", "queued", "pending", "unknown", "inactive")
        records = [record(1, B)]
        records += [record(i, C, state) for i, state in enumerate(states, start=2)]
        records.append(record(9, C))
        with patch.object(dora_metrics, "gh_api_get",
                          side_effect=[{"ahead_by": 10}, {"ahead_by": 4}]) as api:
            unresolved, report = score_and_report(records, A)
        self.assertEqual(api.call_args_list,
                         [call(f"repos/{REPO}/compare/{A}...{B}"),
                          call(f"repos/{REPO}/compare/{B}...{C}")])
        self.assert_scored(records, unresolved, report,
                           [10, *([None] * len(states)), 4], 7.0, 10, True, 0)

    def test_skipped_rows_before_first_success_do_not_supply_predecessor(self):
        records = [record(1, A, "failure"), record(2, A, "unknown"), record(3, B)]
        with patch.object(dora_metrics, "gh_api_get") as api:
            unresolved, report = score_and_report(records, None)
        api.assert_not_called()
        self.assert_scored(records, unresolved, report, [None, None, None],
                           None, None, None, 1)

    def test_non_success_between_same_sha_successes_keeps_zero(self):
        records = [record(1, B), record(2, C, "failure"), record(3, B)]
        with patch.object(dora_metrics, "gh_api_get", return_value={"ahead_by": 10}) as api:
            unresolved, report = score_and_report(records, A)
        api.assert_called_once_with(f"repos/{REPO}/compare/{A}...{B}")
        self.assert_scored(records, unresolved, report, [10, None, 0], 5.0, 10, True, 0)


if __name__ == "__main__":
    unittest.main()

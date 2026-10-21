"""Offline deploy-ID fixtures; stdlib only, no GitHub API (gh is stubbed)."""

import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import deploy_id
from build_identity import (
    count_deployments_for_date,
    deploy_id_sort_key,
    format_deploy_id,
    is_deploy_id_shape,
    parse_deploy_id,
)

SHA = "abcdef0123456789abcdef0123456789abcdef01"
SHA7 = SHA[:7]
DATE = "2026.10.21"


def row(created_at, environment="staging", description=f"{DATE}.1+{SHA7}"):
    # A recorded deploy carries its ID as the description; pass description=None
    # for an automatic `environment:` row, which carries none and never counts.
    return {"created_at": created_at, "environment": environment,
            "description": description}


class FormatTests(unittest.TestCase):
    def test_round_trip(self):
        value = format_deploy_id(DATE, 1, SHA7)
        self.assertEqual(value, f"{DATE}.1+{SHA7}")
        self.assertTrue(is_deploy_id_shape(value))
        self.assertEqual(parse_deploy_id(value), (DATE, 1, SHA7))

    def test_rejects_non_canonical_parts(self):
        for date_text, n, sha7 in (
            ("2026.1.1", 1, SHA7),      # unpadded date
            ("2026.13.01", 1, SHA7),    # month 13
            ("2026.02.30", 1, SHA7),    # February 30
            ("2026.10.21", 0, SHA7),    # N starts at 1
            ("2026.10.21", -1, SHA7),
            ("2026.10.21", "1", SHA7),  # bool is not int either
            ("2026.10.21", True, SHA7),
            ("2026.10.21", 1_000_000, SHA7),  # N over the length bound
            (DATE, 1, SHA7.upper()),    # uppercase short SHA
            (DATE, 1, SHA7[:6]),
            (DATE, 1, "g" * 7),         # non-hex short SHA
            (None, 1, SHA7),
            (DATE, 1, None),
        ):
            with self.subTest(date_text=date_text, n=n, sha7=sha7):
                with self.assertRaises(ValueError):
                    format_deploy_id(date_text, n, sha7)

    def test_rejects_non_canonical_ids(self):
        for value in (
            f"{DATE}.01+{SHA7}",        # leading zero on N
            f"{DATE}.0+{SHA7}",         # N zero
            f"2026.1.1.1+{SHA7}",       # unpadded date
            f"2026.13.01.1+{SHA7}",     # bad month
            f"2026.02.30.1+{SHA7}",     # bad day
            f"{DATE}.1+{SHA7.upper()}",  # uppercase short SHA
            f"{DATE}.1+{SHA7[:6]}x",     # non-hex short SHA
            f"{DATE}.1+{SHA7} ",         # trailing space
            f" {DATE}.1+{SHA7}",
            f"{DATE}+{SHA7}",            # missing N
            f"{DATE}.1",                 # missing short SHA
            "unknown",                  # the unstamped marker is not an ID
            "",
            None,
            42,
        ):
            with self.subTest(value=value):
                self.assertFalse(is_deploy_id_shape(value))
                with self.assertRaises(ValueError):
                    parse_deploy_id(value)

    def test_leap_day_counts_only_on_leap_years(self):
        self.assertTrue(is_deploy_id_shape(f"2024.02.29.1+{SHA7}"))
        self.assertFalse(is_deploy_id_shape(f"2026.02.29.1+{SHA7}"))


class OrderingTests(unittest.TestCase):
    def test_orders_by_date_then_sequence_then_sha(self):
        ids = [
            f"2026.10.21.2+{SHA7}",
            f"2026.10.20.9+{'f' * 7}",
            f"2026.10.21.10+{'0' * 7}",
            f"2026.10.21.2+{'0' * 7}",
            f"2026.10.21.1+{'f' * 7}",
        ]
        self.assertEqual(sorted(ids, key=deploy_id_sort_key), [
            f"2026.10.20.9+{'f' * 7}",
            f"2026.10.21.1+{'f' * 7}",
            f"2026.10.21.2+{'0' * 7}",
            f"2026.10.21.2+{SHA7}",
            f"2026.10.21.10+{'0' * 7}",
        ])

    def test_sort_key_rejects_non_canonical(self):
        with self.assertRaises(ValueError):
            deploy_id_sort_key("unknown")


class CountTests(unittest.TestCase):
    def test_counts_only_the_utc_date(self):
        deployments = [
            row("2026-10-21T00:00:00Z"),
            row("2026-10-21T23:59:59Z"),
            row("2026-10-20T23:59:59Z"),
            row("2026-10-22T00:00:00Z"),
            # 2026-10-21 01:00 +02:00 is still 2026-10-20 in UTC.
            row("2026-10-21T01:00:00+02:00"),
            # 2026-10-20 23:00 -02:00 is already 2026-10-21 in UTC.
            row("2026-10-20T23:00:00-02:00"),
        ]
        self.assertEqual(count_deployments_for_date(deployments, "2026.10.21"), 3)
        self.assertEqual(count_deployments_for_date(deployments, "2026.10.20"), 2)
        self.assertEqual(count_deployments_for_date([], "2026.10.21"), 0)

    def test_malformed_rows_fail_closed(self):
        for deployments in (
            [{"environment": "staging"}],
            [{"created_at": None}],
            [{"created_at": ""}],
            [{"created_at": "not-a-timestamp"}],
            [{"created_at": "2026-10-21T00:00:00"}],  # no time zone
            ["2026-10-21T00:00:00Z"],
        ):
            with self.subTest(deployments=deployments):
                with self.assertRaises(ValueError):
                    count_deployments_for_date(deployments, "2026.10.21")

    def test_ignores_same_day_rows_without_a_recorded_deploy_id(self):
        # Actions auto-creates Deployment rows for `environment:` jobs (each
        # deploy job's own run included); those carry no deploy ID and must
        # never count toward N, or the first deploy of the day mints N=2.
        deployments = [
            row("2026-10-21T08:00:00Z"),
            row("2026-10-21T09:00:00Z", description=None),
            row("2026-10-21T10:00:00Z", description=""),
            row("2026-10-21T11:00:00Z", description="not-an-id"),
            row("2026-10-21T12:00:00Z", description="unknown"),
            {"created_at": "2026-10-21T13:00:00Z", "environment": "staging"},
        ]
        self.assertEqual(count_deployments_for_date(deployments, "2026.10.21"), 1)


class NextTests(unittest.TestCase):
    def test_first_deploy_of_the_day_is_one(self):
        self.assertEqual(
            deploy_id.next_deploy_id([], DATE, SHA), f"{DATE}.1+{SHA7}")

    def test_counts_only_today(self):
        deployments = [row("2026-10-21T08:00:00Z"), row("2026-10-21T09:00:00Z"),
                       row("2026-10-20T09:00:00Z")]
        self.assertEqual(
            deploy_id.next_deploy_id(deployments, DATE, SHA), f"{DATE}.3+{SHA7}")

    def test_rejects_a_non_sha(self):
        with self.assertRaises(deploy_id.DeployIdError):
            deploy_id.next_deploy_id([], DATE, SHA7)


class RequestTests(unittest.TestCase):
    def test_deployment_carries_the_id_not_a_tag(self):
        body = deploy_id.deployment_request("staging", SHA, f"{DATE}.1+{SHA7}")
        self.assertEqual(body, {
            "ref": SHA,
            "task": "deploy",
            "environment": "staging",
            "production_environment": False,
            "auto_merge": False,
            "required_contexts": [],
            "description": f"{DATE}.1+{SHA7}",
            "payload": {"deploy_id": f"{DATE}.1+{SHA7}"},
        })
        self.assertNotIn("tag_name", json.dumps(body))

    def test_production_marks_serving_state(self):
        body = deploy_id.deployment_request("production", SHA, f"{DATE}.1+{SHA7}")
        self.assertEqual(body["environment"], "production")
        self.assertTrue(body["production_environment"])

    def test_rollback_names_itself(self):
        body = deploy_id.deployment_request(
            "production", SHA, f"{DATE}.3+{SHA7}", rollback=True)
        self.assertEqual(body["payload"],
                         {"deploy_id": f"{DATE}.3+{SHA7}", "rollback": True})

    def test_refuses_a_mismatched_commit(self):
        with self.assertRaises(deploy_id.DeployIdError):
            deploy_id.deployment_request("staging", SHA, f"{DATE}.1+{'f' * 7}")
        with self.assertRaises(deploy_id.DeployIdError):
            deploy_id.deployment_request("canary", SHA, f"{DATE}.1+{SHA7}")

    def test_status_carries_state_and_links(self):
        body = deploy_id.deployment_status_request(
            "success", f"{DATE}.1+{SHA7}",
            log_url="https://example.invalid/run",
            environment_url="https://example.invalid/worker")
        self.assertEqual(body["state"], "success")
        self.assertEqual(body["description"], f"{DATE}.1+{SHA7}")
        self.assertEqual(body["log_url"], "https://example.invalid/run")
        with self.assertRaises(deploy_id.DeployIdError):
            deploy_id.deployment_status_request("pending", f"{DATE}.1+{SHA7}")


class GhTests(unittest.TestCase):
    def run_cli(self, argv, outputs=(), failures=()):
        """Run main() with a stubbed `gh api` returning one JSON value per call."""
        calls = []

        def fake_run(cmd, **kwargs):
            calls.append((cmd, kwargs.get("input")))
            index = len(calls) - 1
            if index in failures:
                return subprocess.CompletedProcess(cmd, 1, "", "refused")
            return subprocess.CompletedProcess(cmd, 0, json.dumps(outputs[index]), "")

        out = io.StringIO()
        with patch.object(subprocess, "run", side_effect=fake_run):
            with redirect_stdout(out):
                code = deploy_id.main(argv)
        return code, out.getvalue(), calls

    def test_next_lists_pages_and_prints_the_env_line(self):
        page = [row("2026-10-21T08:00:00Z")]
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, calls = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=(page,),
            )
        # A short first page ends the listing: one API call, N = 2.
        self.assertEqual(code, 0)
        self.assertEqual(out, f"DEPLOY_ID={DATE}.2+{SHA7}\n")
        self.assertEqual(len(calls), 1)
        self.assertIn("environment=staging", str(calls[0][0]))

    def test_next_stops_at_the_first_row_older_than_the_date(self):
        full = [row("2026-10-21T08:00:00Z")] * 100
        older = [row("2026-10-20T08:00:00Z")]
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, calls = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=(full, older),
            )
        # Total history never matters: two pages, N counts today only.
        self.assertEqual(code, 0)
        self.assertEqual(out, f"DEPLOY_ID={DATE}.101+{SHA7}\n")
        self.assertEqual(len(calls), 2)

    def test_next_stops_mid_page_at_an_older_row_without_fetching_more(self):
        # The early stop ends the listing at the first older row even when
        # more rows follow on the same page: one API call, N counts only the
        # pre-stop rows. Without the stop this fixture fetches page 2 and
        # mints N from the whole page, so the test pins the stop itself, not
        # the short-page rule.
        page = ([row("2026-10-21T08:00:00Z"), row("2026-10-20T08:00:00Z")]
                + [row("2026-10-21T09:00:00Z")] * 98)
        older = [row("2026-10-20T08:00:00Z")]
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, calls = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=(page, older),
            )
        self.assertEqual(code, 0)
        self.assertEqual(out, f"DEPLOY_ID={DATE}.2+{SHA7}\n")
        self.assertEqual(len(calls), 1)

    def test_next_ignores_future_rows_without_counting_them(self):
        rows = [row("2026-10-22T08:00:00Z"), row("2026-10-21T08:00:00Z")]
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, _ = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=(rows,),
            )
        self.assertEqual(code, 0)
        self.assertEqual(out, f"DEPLOY_ID={DATE}.2+{SHA7}\n")

    def test_next_refuses_a_malformed_listing_row(self):
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, _ = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=([{"environment": "staging"}],),
            )
        self.assertEqual(code, 1)
        self.assertIn("created_at", out)

    def test_next_refuses_past_the_page_cap(self):
        full = [row("2026-10-21T08:00:00Z")] * 100
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, _ = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=(full,) * 6,
            )
        self.assertEqual(code, 1)
        self.assertIn("too many deployments", out)

    def test_next_offers_an_offline_fixture(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Path(tmp) / "deployments.json"
            fixture.write_text(json.dumps([row("2026-10-21T08:00:00Z")]))
            code, out, calls = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA,
                 "--date", DATE, "--deployments-json", str(fixture)],
                outputs=(),
            )
        self.assertEqual(code, 0)
        self.assertEqual(out, f"DEPLOY_ID={DATE}.2+{SHA7}\n")
        self.assertEqual(calls, [])

    def test_record_posts_deployment_then_status(self):
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, calls = self.run_cli(
                ["record", "--environment", "staging", "--sha", SHA,
                 "--deploy-id", f"{DATE}.1+{SHA7}", "--state", "success",
                 "--run-url", "https://example.invalid/run"],
                outputs=({"id": 42}, {"state": "success"}),
            )
        self.assertEqual(code, 0)
        self.assertEqual(out, "deployment_id=42 state=success\n")
        self.assertEqual(len(calls), 2)
        create = json.loads(calls[0][1])
        self.assertEqual(create["environment"], "staging")
        self.assertEqual(create["payload"], {"deploy_id": f"{DATE}.1+{SHA7}"})
        status = json.loads(calls[1][1])
        self.assertEqual(status["state"], "success")

    def test_record_without_a_repo_refuses(self):
        with patch.dict("os.environ", {}, clear=True):
            code, out, _ = self.run_cli(
                ["record", "--environment", "staging", "--sha", SHA,
                 "--deploy-id", f"{DATE}.1+{SHA7}", "--state", "success"],
                outputs=(),
            )
        self.assertEqual(code, 1)
        self.assertIn("GITHUB_REPOSITORY", out)

    def test_api_failures_stay_fixed_vocabulary(self):
        with patch.dict("os.environ", {"GITHUB_REPOSITORY": "owner/repo"}, clear=False):
            code, out, _ = self.run_cli(
                ["next", "--environment", "staging", "--sha", SHA, "--date", DATE],
                outputs=({},),
                failures=(0,),
            )
        self.assertEqual(code, 1)
        self.assertEqual(out, "deploy_id failed: github api refused the deployments call\n")


if __name__ == "__main__":
    unittest.main()

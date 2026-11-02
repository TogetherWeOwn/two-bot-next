"""Acceptance: staging rollout receipts are validated before acting.

BUILDER CONTRACT: this file is frozen. Build on the test card's branch, keep
every test here verbatim, and make the suite green by changing production code
only — validate the local baseline receipt in `scripts/staging_rollout.py`
before `select_rollout` / `receipt` / `verify` act on it:

- Every baseline member (`started`, `application_id`, `namespace_id`,
  `rollout_ids`, `revision`, `build_id`) is type- and shape-checked on load,
  like `ProvenanceReceipt.__post_init__` in
  `scripts/worker_container_rollback.py` validates every member.
- `rollout_ids` must be a list of identifier-shaped strings; a string,
  missing key, non-list, or non-string member refuses instead of silently
  flipping list- to substring-membership in `select_rollout`.
- Refusals raise `GateError("invalid_baseline_receipt")` — one fixed token,
  no upstream text, no IDs or digests in the message.
- Valid receipts pass through byte-identically (see GREEN controls).

Suite map (all offline: stdlib only, no Cloudflare, Docker or waits):

- string/missing/non-list/non-string rollout_ids (RED now: silent
  substring misprediction, KeyError, or wrong code).
- mistyped started/revision/build_id/application_id/namespace_id (RED now:
  TypeError or silent pass-through).
- file-based receipt()/verify() baseline refusal (RED now: mistyped members
  ignored or wrong exception).
- GREEN controls (pass before and after): valid baselines select/skip
  correctly, receipt mode accepts a valid baseline, and the provenance
  receipt still validates every member.
"""

import argparse
import contextlib
import copy
from datetime import datetime, timezone
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first,
# then imports the sibling module under test (no linter runs on scripts/).
import staging_rollout as rollout
import worker_container_rollback as rollback

# Fixed refusal vocabulary for a malformed local baseline receipt. The builder
# adds this token; today no baseline shape check exists.
BASELINE_CODE = "invalid_baseline_receipt"

STARTED = 1_800_000_000.0
VERSION = "12345678-1234-4234-8234-123456789abc"
REVISION = "a" * 40
BUILD_ID = "123456-2"
ACCOUNT = "b" * 32
NAMESPACE = "c" * 64
APPLICATION_ID = "app_opaque-123"
OLD_ROLLOUT = "rollout_before-123"
NEW_ROLLOUT = "rollout_after-456"
REPOSITORY = f"registry.cloudflare.com/{ACCOUNT}/two-bot-next-twobotcontainer-staging"
IMAGE = REPOSITORY + "@sha256:" + "d" * 64
OLD_IMAGE = REPOSITORY + "@sha256:" + "e" * 64
SENTINEL = "RAW_SECRET_SENTINEL_never_disclose_database_password"


def date(offset=0):
    return datetime.fromtimestamp(STARTED + offset, timezone.utc).isoformat()


def valid_baseline():
    return {
        "started": STARTED,
        "application_id": APPLICATION_ID,
        "namespace_id": NAMESPACE,
        "rollout_ids": [OLD_ROLLOUT],
        "revision": REVISION,
        "build_id": BUILD_ID,
        "deploy_id": "2026.10.21.1+aaaaaaa",
    }


def new_row(row_id=NEW_ROLLOUT):
    return {
        "id": row_id,
        "created_at": date(),
        "strategy": "rolling",
        "kind": "full_auto",
        "status": "completed",
        "target_version": 8,
        "target_configuration": {"image": IMAGE},
        "current_version": 7,
        "current_configuration": {"image": OLD_IMAGE},
        "health": {"instances": {
            "active": 1, "healthy": 1, "failed": 0, "starting": 0, "scheduling": 0,
        }},
        "progress": {"total_steps": 2, "current_step": 2,
                     "total_instances": 1, "updated_instances": 1},
        "steps": [{"status": "completed"}, {"status": "completed"}],
    }


def deploy_records():
    return [
        {
            "type": "wrangler-session", "version": 1, "wrangler_version": "4.152.0",
            "command_line_args": ["deploy", "--config", "staging-deploy.json",
                                  "--env", "staging"],
        },
        {
            "type": "deploy", "version": 1,
            "worker_name": "two-bot-next-staging",
            "wrangler_environment": "staging", "worker_name_overridden": False,
            "timestamp": date(), "version_id": VERSION,
        },
    ]


class OfflineTestCase(unittest.TestCase):
    def setUp(self):
        opener = Mock()
        opener.open.side_effect = AssertionError("live HTTP forbidden in offline tests")
        self.enterContext(patch.object(rollout, "build_opener", return_value=opener))
        self.enterContext(patch.object(rollout.subprocess, "run",
                                      side_effect=AssertionError("live Docker forbidden")))

    def assert_baseline_gate(self, function, *args):
        """Assert a fixed-vocabulary baseline refusal.

        Today these paths raise KeyError/TypeError, raise a wrong GateError
        code, or silently mispredict — each is a behavioral RED recorded as
        an assertion failure, never an error.
        """
        try:
            result = function(*args)
        except rollout.GateError as error:
            self.assertEqual(str(error), BASELINE_CODE)
            self.assertNotIn(SENTINEL, str(error))
            return
        except (KeyError, TypeError, AttributeError, ValueError) as error:
            self.fail(
                f"raises {type(error).__name__} ({error}), not fixed-vocabulary "
                f"GateError({BASELINE_CODE})"
            )
        self.fail(
            f"silently accepts a malformed baseline (returned {result!r}), "
            f"expected GateError({BASELINE_CODE})"
        )


class StringRolloutIdsTests(OfflineTestCase):
    def test_string_rollout_ids_refuses_instead_of_substring_match(self):
        # A hand-edited receipt with a string `rollout_ids` flips
        # `identifier(...) in baseline["rollout_ids"]` from list- to
        # substring-membership. "rollout" is a substring of the stored
        # string, so today the new row is silently skipped as "already
        # seen" and select_rollout returns None.
        baseline = valid_baseline()
        baseline["rollout_ids"] = OLD_ROLLOUT
        row = new_row("rollout")
        self.assertIn(row["id"], baseline["rollout_ids"])
        self.assert_baseline_gate(rollout.select_rollout, [row], baseline, IMAGE)

    def test_string_rollout_ids_refuses_for_exact_and_partial_matches(self):
        for row_id in [OLD_ROLLOUT, "rollout_before", "before-123", "unrelated-id"]:
            with self.subTest(row_id=row_id):
                baseline = valid_baseline()
                baseline["rollout_ids"] = OLD_ROLLOUT
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row(row_id)], baseline, IMAGE
                )

    def test_empty_string_rollout_ids_refuses(self):
        baseline = valid_baseline()
        baseline["rollout_ids"] = ""
        self.assert_baseline_gate(
            rollout.select_rollout, [new_row()], baseline, IMAGE
        )


class MissingAndMistypedRolloutIdsTests(OfflineTestCase):
    def test_missing_rollout_ids_refuses(self):
        baseline = valid_baseline()
        del baseline["rollout_ids"]
        self.assert_baseline_gate(
            rollout.select_rollout, [new_row()], baseline, IMAGE
        )

    def test_non_list_rollout_ids_refuses(self):
        for value in [None, 123, {"id": OLD_ROLLOUT}, (OLD_ROLLOUT,), True]:
            with self.subTest(value=value):
                baseline = valid_baseline()
                baseline["rollout_ids"] = value
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row()], baseline, IMAGE
                )

    def test_non_string_members_refuse(self):
        for members in [[123], [None], [""], [[OLD_ROLLOUT]],
                        [{"id": OLD_ROLLOUT}], ["ok-id", 42], ["bad id!"]]:
            with self.subTest(members=members):
                baseline = valid_baseline()
                baseline["rollout_ids"] = members
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row()], baseline, IMAGE
                )

class MistypedBaselineMemberTests(OfflineTestCase):
    def test_mistyped_started_refuses(self):
        for value in ["2026-10-21T00:00:00Z", "1800000000", None, [STARTED],
                      {"t": STARTED}, True]:
            with self.subTest(value=value):
                baseline = valid_baseline()
                baseline["started"] = value
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row()], baseline, IMAGE
                )

    def test_mistyped_scalar_members_refuse(self):
        cases = [
            ("application_id", 123), ("application_id", None),
            ("application_id", ["x"]),
            ("namespace_id", 42), ("namespace_id", None), ("namespace_id", {}),
            ("revision", 12345), ("revision", None), ("revision", ["a" * 40]),
            ("build_id", 99), ("build_id", None), ("build_id", {"b": 1}),
        ]
        for member, value in cases:
            with self.subTest(member=member, value=value):
                baseline = valid_baseline()
                baseline[member] = value
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row()], baseline, IMAGE
                )

    def test_missing_scalar_members_refuse(self):
        for member in ["started", "application_id", "namespace_id",
                       "revision", "build_id"]:
            with self.subTest(member=member):
                baseline = valid_baseline()
                del baseline[member]
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row()], baseline, IMAGE
                )


class ReceiptModeBaselineTests(OfflineTestCase):
    def setUp(self):
        super().setUp()
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.args = argparse.Namespace(
            receipt=str(self.root / "baseline.json"),
            output=str(self.root / "deploy.ndjson"),
            evidence=str(self.root / "evidence.json"),
        )
        Path(self.args.output).write_text(
            "\n".join(json.dumps(row) for row in deploy_records()) + "\n"
        )
        self.stdout = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.stderr = self.enterContext(contextlib.redirect_stderr(io.StringIO()))

    def write_baseline(self, baseline):
        Path(self.args.receipt).write_text(json.dumps(baseline))

    def test_receipt_mode_string_rollout_ids_refuses(self):
        baseline = valid_baseline()
        baseline["rollout_ids"] = OLD_ROLLOUT
        self.write_baseline(baseline)
        self.assert_baseline_gate(rollout.receipt, self.args)
        self.assertFalse(Path(self.args.evidence).exists())

    def test_receipt_mode_missing_rollout_ids_refuses(self):
        baseline = valid_baseline()
        del baseline["rollout_ids"]
        self.write_baseline(baseline)
        self.assert_baseline_gate(rollout.receipt, self.args)
        self.assertFalse(Path(self.args.evidence).exists())

    def test_receipt_mode_mistyped_members_refuse(self):
        cases = [
            {"started": "not-a-timestamp"},
            {"revision": 12345},
            {"build_id": None},
            {"application_id": ["x"]},
            {"namespace_id": {"n": 1}},
            {"rollout_ids": [42]},
        ]
        for override in cases:
            with self.subTest(override=override):
                baseline = valid_baseline()
                baseline.update(override)
                self.write_baseline(baseline)
                self.assert_baseline_gate(rollout.receipt, self.args)
                self.assertFalse(Path(self.args.evidence).exists())

    def test_receipt_mode_malformed_json_stays_invalid_json(self):
        # Control within the RED class: undecodable receipts keep their
        # existing code; the new baseline code is for decodable-but-wrong
        # shapes only.
        Path(self.args.receipt).write_text("{not json")
        with self.assertRaises(rollout.GateError) as caught:
            rollout.receipt(self.args)
        self.assertEqual(str(caught.exception), "invalid_json")


class ValidBaselineControls(OfflineTestCase):
    def test_valid_baseline_selects_new_rollout(self):
        row = new_row()
        self.assertIs(rollout.select_rollout([row], valid_baseline(), IMAGE), row)

    def test_valid_baseline_skips_known_ids(self):
        row = new_row(OLD_ROLLOUT)
        self.assertIsNone(rollout.select_rollout([row], valid_baseline(), IMAGE))
        self.assertIsNone(rollout.select_rollout([], valid_baseline(), IMAGE))

    def test_valid_baseline_multiple_known_ids(self):
        baseline = valid_baseline()
        baseline["rollout_ids"] = [OLD_ROLLOUT, "another-known-id"]
        self.assertIsNone(
            rollout.select_rollout([new_row("another-known-id")], baseline, IMAGE)
        )
        row = new_row()
        self.assertIs(rollout.select_rollout([row], baseline, IMAGE), row)

    def test_receipt_mode_accepts_valid_baseline(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        args = argparse.Namespace(
            receipt=str(root / "baseline.json"),
            output=str(root / "deploy.ndjson"),
            evidence=str(root / "evidence.json"),
        )
        Path(args.receipt).write_text(json.dumps(valid_baseline()))
        Path(args.output).write_text(
            "\n".join(json.dumps(row) for row in deploy_records()) + "\n"
        )
        with contextlib.redirect_stdout(io.StringIO()):
            rollout.receipt(args)
        self.assertEqual(
            json.loads(Path(args.evidence).read_text()), {"worker_version": VERSION}
        )

    def test_provenance_receipt_still_validates_every_member(self):
        # Contrast control: the rollback receipt this work mirrors already
        # refuses each mistyped member with fixed vocabulary. The staging
        # baseline must match that posture.
        good = {
            "worker": "two-bot-next-staging",
            "application": "two-bot-next-twobotcontainer-staging",
            "version": VERSION,
            "sha": REVISION,
            "image": IMAGE,
        }
        rollback.ProvenanceReceipt(**good)
        bad_members = {
            "worker": (42, "provenance_worker_invalid"),
            "application": (None, "provenance_application_invalid"),
            "version": ("not-a-uuid", "provenance_version_invalid"),
            "sha": ("xyz", "provenance_sha_invalid"),
            "image": ("not-an-image", "provenance_image_invalid"),
        }
        for member, (value, code) in bad_members.items():
            with self.subTest(member=member):
                mutated = dict(good)
                mutated[member] = value
                with self.assertRaises(rollback.GateError) as caught:
                    rollback.ProvenanceReceipt(**mutated)
                self.assertEqual(str(caught.exception), code)


# Removing deploy-ID validation, accepting non-string/malformed deploy IDs,
# checking only populated rollout lists, or checking after Docker/API access
# must fail these tests. Expected values are independent literal fixtures.
MISTYPED_DEPLOY_IDS = (
    None, True, False, 20261021, 1.5,
    ["2026.10.21.1+aaaaaaa"], {"id": "2026.10.21.1+aaaaaaa"},
)
MALFORMED_DEPLOY_IDS = (
    "", " ", "unknown", "123456-2", "2026.10.21.1", "2026.10.21+aaaaaaa",
    "2026-10-21.1+aaaaaaa", "2026.10.21.x+aaaaaaa",
    "2026.10.21.0+aaaaaaa", "2026.10.21.01+aaaaaaa",
    "2026.13.01.1+aaaaaaa", "2026.02.30.1+aaaaaaa",
    "2026.10.21.1+aaaaaa", "2026.10.21.1+aaaaaaaa",
    "2026.10.21.1+ggggggg", "2026.10.21.1+AAAAAAA",
    " 2026.10.21.1+aaaaaaa", "2026.10.21.1+aaaaaaa ",
    "2026.10.21.1+aaaaaaa\n", SENTINEL,
)


def missing_deploy_id():
    baseline = valid_baseline()
    del baseline["deploy_id"]
    return baseline


def with_deploy_id(value):
    baseline = valid_baseline()
    baseline["deploy_id"] = value
    return baseline


class DeployIdBaselineAcceptanceTests(OfflineTestCase):
    """Require a canonical deploy ID before selection, receipt, or verification."""

    def assert_selection_refuses(self, baseline):
        for rows in ([], [new_row()]):
            with self.subTest(rows=len(rows)):
                self.assert_baseline_gate(
                    rollout.select_rollout, rows, baseline, IMAGE
                )

    def assert_file_mode_refuses(self, mode, baseline):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        args = argparse.Namespace(
            receipt=str(root / "baseline.json"),
            output=str(root / "deploy.ndjson"),
            evidence=str(root / "evidence.json"),
        )
        baseline_bytes = json.dumps(baseline).encode()
        output_bytes = ("\n".join(
            json.dumps(row) for row in deploy_records()
        ) + "\n").encode()
        Path(args.receipt).write_bytes(baseline_bytes)
        Path(args.output).write_bytes(output_bytes)
        stdout = io.StringIO()
        stderr = io.StringIO()
        client = Mock(deadline=110.0)
        client.api.side_effect = AssertionError("API before baseline refusal")
        client.request.side_effect = AssertionError("HTTP before baseline refusal")
        environment = {
            "GITHUB_SHA": REVISION,
            "GITHUB_RUN_ID": "123456",
            "GITHUB_RUN_ATTEMPT": "2",
            "STAGING_URL": "https://two-bot-next-staging.offline-fixture.workers.dev",
            "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
            "CLOUDFLARE_API_TOKEN": SENTINEL,
        }
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr), \
                patch.dict(os.environ, environment, clear=True), \
                patch.object(rollout, "docker_image", side_effect=AssertionError(
                    "Docker before baseline refusal")) as docker, \
                patch.object(rollout.time, "sleep", side_effect=AssertionError(
                    "wait before baseline refusal")):
            if mode == "verify":
                self.assert_baseline_gate(rollout.verify, args, client)
            else:
                self.assert_baseline_gate(rollout.receipt, args)
        docker.assert_not_called()
        client.api.assert_not_called()
        client.request.assert_not_called()
        self.assertFalse(Path(args.evidence).exists())
        self.assertEqual(Path(args.receipt).read_bytes(), baseline_bytes)
        self.assertEqual(Path(args.output).read_bytes(), output_bytes)
        self.assertNotIn(SENTINEL, stdout.getvalue())
        self.assertNotIn(SENTINEL, stderr.getvalue())

    def test_select_missing_deploy_id_refuses_even_without_rows(self):
        self.assert_selection_refuses(missing_deploy_id())

    def test_select_mistyped_deploy_id_refuses_even_without_rows(self):
        for value in MISTYPED_DEPLOY_IDS:
            with self.subTest(value=value):
                self.assert_selection_refuses(with_deploy_id(value))

    def test_select_malformed_deploy_id_refuses_even_without_rows(self):
        for value in MALFORMED_DEPLOY_IDS:
            with self.subTest(value=value):
                self.assert_selection_refuses(with_deploy_id(value))

    def test_receipt_missing_deploy_id_refuses_without_evidence(self):
        self.assert_file_mode_refuses("receipt", missing_deploy_id())

    def test_receipt_mistyped_deploy_id_refuses_without_evidence(self):
        for value in MISTYPED_DEPLOY_IDS:
            with self.subTest(value=value):
                self.assert_file_mode_refuses("receipt", with_deploy_id(value))

    def test_receipt_malformed_deploy_id_refuses_without_evidence(self):
        for value in MALFORMED_DEPLOY_IDS:
            with self.subTest(value=value):
                self.assert_file_mode_refuses("receipt", with_deploy_id(value))

    def test_verify_missing_deploy_id_refuses_before_docker_or_api(self):
        self.assert_file_mode_refuses("verify", missing_deploy_id())

    def test_verify_mistyped_deploy_id_refuses_before_docker_or_api(self):
        for value in MISTYPED_DEPLOY_IDS:
            with self.subTest(value=value):
                self.assert_file_mode_refuses("verify", with_deploy_id(value))

    def test_verify_malformed_deploy_id_refuses_before_docker_or_api(self):
        for value in MALFORMED_DEPLOY_IDS:
            with self.subTest(value=value):
                self.assert_file_mode_refuses("verify", with_deploy_id(value))


if __name__ == "__main__":
    unittest.main()

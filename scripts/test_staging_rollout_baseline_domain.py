"""Baseline domain follow-ups: `started` must be finite and positive, and
file-based `receipt`/`verify` must refuse non-object JSON with the single
baseline token instead of `invalid_api_schema`.

Fresh file for the review round on top of the frozen receipt-validation
acceptance suite; every case here is offline (stdlib only, no Cloudflare,
Docker or waits).
"""

import argparse
import contextlib
import io
import json
import math
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first,
# then imports the sibling module under test (no linter runs on scripts/).
import staging_rollout as rollout

BASELINE_CODE = "invalid_baseline_receipt"

STARTED = 1_800_000_000.0
VERSION = "12345678-1234-4234-8234-123456789abc"
REVISION = "a" * 40
BUILD_ID = "123456-2"
NAMESPACE = "c" * 64
APPLICATION_ID = "app_opaque-123"
OLD_ROLLOUT = "rollout_before-123"
NEW_ROLLOUT = "rollout_after-456"
ACCOUNT = "b" * 32
REPOSITORY = f"registry.cloudflare.com/{ACCOUNT}/two-bot-next-twobotcontainer-staging"
IMAGE = REPOSITORY + "@sha256:" + "d" * 64
OLD_IMAGE = REPOSITORY + "@sha256:" + "e" * 64


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


# 2027-06-01 is past STARTED (1.8e9 = 2027-01-15T08:00:00Z), so the
# timestamp floor passes whenever the baseline itself is in-domain.
def new_row(row_id=NEW_ROLLOUT, created_at="2027-06-01T00:00:00Z"):
    return {
        "id": row_id,
        "created_at": created_at,
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
    }


class OfflineTestCase(unittest.TestCase):
    def setUp(self):
        opener = Mock()
        opener.open.side_effect = AssertionError("live HTTP forbidden in offline tests")
        self.enterContext(patch.object(rollout, "build_opener", return_value=opener))
        self.enterContext(patch.object(rollout.subprocess, "run",
                                      side_effect=AssertionError("live Docker forbidden")))

    def assert_baseline_gate(self, function, *args):
        try:
            result = function(*args)
        except rollout.GateError as error:
            self.assertEqual(str(error), BASELINE_CODE)
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


class StartedDomainTests(OfflineTestCase):
    def test_out_of_domain_started_refuses(self):
        for value in [-1, 0, 0.0, -0.5, math.nan, math.inf, -math.inf,
                      -(10 ** 400)]:
            with self.subTest(value=value):
                baseline = valid_baseline()
                baseline["started"] = value
                self.assert_baseline_gate(
                    rollout.select_rollout, [new_row()], baseline, IMAGE
                )

    def test_out_of_domain_started_refuses_in_receipt_mode(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        args = argparse.Namespace(
            receipt=str(root / "baseline.json"),
            output=str(root / "deploy.ndjson"),
            evidence=str(root / "evidence.json"),
        )
        for value in [-1, 0, math.nan, math.inf, -(10 ** 400)]:
            with self.subTest(value=value):
                baseline = valid_baseline()
                baseline["started"] = value
                Path(args.receipt).write_text(json.dumps(baseline))
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assert_baseline_gate(rollout.receipt, args)
                self.assertFalse(Path(args.evidence).exists())

    def test_negative_started_no_longer_selects_a_pre_baseline_row(self):
        # Regression shape: with started=-1 the timestamp floor accepted a
        # rollout created before the real baseline as new.
        baseline = valid_baseline()
        baseline["started"] = -1
        row = new_row(created_at="1970-01-01T00:00:01Z")
        self.assert_baseline_gate(rollout.select_rollout, [row], baseline, IMAGE)

    def test_huge_positive_integer_started_does_not_crash(self):
        # math.isfinite() raises OverflowError on ints beyond the float
        # range, so only floats take the finiteness check: a huge positive
        # int passes validation exactly as before, then loses the timestamp
        # floor under the fixed stale token instead of a traceback.
        baseline = valid_baseline()
        baseline["started"] = 10 ** 400
        self.assertIs(rollout.validate_baseline(baseline), baseline)
        with self.assertRaises(rollout.GateError) as caught:
            rollout.select_rollout([new_row()], baseline, IMAGE)
        self.assertEqual(str(caught.exception), "stale_new_rollout")

    def test_finite_positive_started_still_selects(self):
        for value in [1, 0.001, STARTED, 1_800_000_000]:
            with self.subTest(value=value):
                baseline = valid_baseline()
                baseline["started"] = value
                row = new_row()
                self.assertIs(rollout.select_rollout([row], baseline, IMAGE), row)


class NonObjectBaselineFileTests(OfflineTestCase):
    def setUp(self):
        super().setUp()
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.args = argparse.Namespace(
            receipt=str(self.root / "baseline.json"),
            output=str(self.root / "deploy.ndjson"),
            evidence=str(self.root / "evidence.json"),
        )
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.enterContext(contextlib.redirect_stderr(io.StringIO()))

    def test_receipt_mode_non_object_json_refuses_with_baseline_token(self):
        for raw in ["[1, 2]", "null", '"just-a-string"', "42", "true"]:
            with self.subTest(raw=raw):
                Path(self.args.receipt).write_text(raw)
                self.assert_baseline_gate(rollout.receipt, self.args)
                self.assertFalse(Path(self.args.evidence).exists())

    def test_verify_mode_non_object_json_refuses_with_baseline_token(self):
        # The baseline is read before any deploy record or client use, so a
        # bare client double suffices: refusal precedes every read.
        for raw in ["[1, 2]", "null", '"just-a-string"']:
            with self.subTest(raw=raw):
                Path(self.args.receipt).write_text(raw)
                self.assert_baseline_gate(rollout.verify, self.args, Mock())

    def test_receipt_mode_object_baseline_still_validates_members(self):
        baseline = valid_baseline()
        baseline["rollout_ids"] = OLD_ROLLOUT
        Path(self.args.receipt).write_text(json.dumps(baseline))
        self.assert_baseline_gate(rollout.receipt, self.args)
        self.assertFalse(Path(self.args.evidence).exists())


if __name__ == "__main__":
    unittest.main()

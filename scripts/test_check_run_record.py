#!/usr/bin/env python3
"""Offline tests for the staging E2E run-record validator (stdlib only)."""

import copy
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_run_record as checker  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = json.loads(
    (ROOT / "docs" / "staging-e2e-run-record.schema.json").read_text(
        encoding="utf-8"
    )
)
MOCK = json.loads(
    (
        ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json"
    ).read_text(encoding="utf-8")
)


def fresh_record(**overrides):
    record = copy.deepcopy(MOCK)
    record.update(overrides)
    return record


class RunRecordValidatorTests(unittest.TestCase):
    def test_mock_fixture_valid_with_allow_mock(self):
        self.assertEqual(checker.validate(MOCK, SCHEMA, allow_mock=True), [])

    def test_mock_flag_rejected_as_evidence_without_allow_mock(self):
        errors = checker.validate(MOCK, SCHEMA)
        self.assertTrue(
            any("$.mock" in error for error in errors),
            f"mock record must fail closed without --allow-mock: {errors}",
        )

    def test_real_shaped_record_valid(self):
        record = fresh_record(mock=False)
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_missing_revision_fails(self):
        record = fresh_record()
        del record["deployment"]["revision"]
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("revision" in error for error in errors),
            f"missing revision must fail: {errors}",
        )

    def test_short_revision_fails(self):
        record = fresh_record()
        record["deployment"]["revision"] = "c3efe26b"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("revision" in error for error in errors),
            f"short revision must fail: {errors}",
        )

    def test_fail_without_failure_signature_fails(self):
        record = fresh_record()
        for command in record["commands"]:
            if command["result"] == "fail":
                del command["failure_signature"]
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("failure_signature" in error for error in errors),
            f"fail without signature must fail: {errors}",
        )

    def test_negative_duration_fails(self):
        record = fresh_record()
        record["commands"][0]["duration_ms"] = -5
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("duration_ms" in error for error in errors),
            f"negative duration must fail: {errors}",
        )

    def test_internal_tracker_id_rejected(self):
        record = fresh_record()
        record["verdict"]["summary"] = "See TOG-12345 for the follow-up."
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("public-safety" in error for error in errors),
            f"internal tracker ID must fail: {errors}",
        )

    def test_secret_marker_rejected(self):
        # Slack-style fake: caught by the validator, too short for the
        # strict gitleaks slack rule, so this fixture scans clean.
        record = fresh_record()
        record["tester"]["identity"] = "tester with xoxb-fake-test-sentinel"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("public-safety" in error for error in errors),
            f"secret marker must fail: {errors}",
        )

    def test_production_target_rejected(self):
        record = fresh_record()
        record["environment"]["target"] = "production"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(errors, "production target must fail")

    def test_cli_accepts_mock_with_flag(self):
        fixture = (
            ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json"
        )
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            out = Path(tmp) / "out.txt"
            code = checker.main(
                ["--record", str(fixture), "--allow-mock"]
            )
            out.write_text(str(code), encoding="utf-8")
            self.assertEqual(code, 0)
            self.assertEqual(out.read_text(encoding="utf-8"), "0")

    def test_cli_rejects_mock_without_flag(self):
        fixture = (
            ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json"
        )
        self.assertEqual(checker.main(["--record", str(fixture)]), 1)


if __name__ == "__main__":
    unittest.main()

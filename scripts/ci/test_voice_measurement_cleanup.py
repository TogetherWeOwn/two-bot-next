"""Contract tests for staging measurement cleanup evidence."""

import json
import tempfile
import unittest
from copy import deepcopy
from pathlib import Path

from voice_measurement_cleanup import cleanup_verified, read_cleanup_receipt


class VoiceMeasurementCleanupTests(unittest.TestCase):
    def test_cleanup_requires_matching_successful_release(self):
        correlation = {"environment": "staging", "runId": "123", "runAttempt": 1}
        use = dict(correlation, expiresAt=123456, acquiredAt=120000, phase="released", sha="a" * 40,
                   deploymentId="deploy", epoch=1)
        records = [
            dict(correlation, schema="two-bot-staging-pipeline.v1", action="attempt", operation="finish",
                 observedAt="1970-01-01T00:02:01.000Z"),
            dict(correlation, schema="two-bot-staging-use.v1", action="status",
                 observedAt="1970-01-01T00:02:02.000Z", sha="a" * 40, deploymentId="deploy",
                 epoch=1, running=True, use=dict(use, phase="active")),
            dict(correlation, schema="two-bot-staging-use.v1", action="release",
                 observedAt="1970-01-01T00:02:03.000Z", sha="a" * 40, deploymentId="deploy",
                 epoch=1, running=False, use=use),
        ]
        args = ("123", 1, 123456, "a" * 40, "deploy")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "receipt.jsonl"
            path.write_text("\n".join(map(json.dumps, records)) + "\n", encoding="utf-8")
            self.assertTrue(cleanup_verified(read_cleanup_receipt(path), *args))
        for index, record in enumerate(records):
            for key, mismatches in (("runId", ("124", 123)), ("runAttempt", (2, True, 1.0, "1"))):
                with self.subTest(row=record["action"], field=key, missing=True):
                    changed = deepcopy(records)
                    del changed[index][key]
                    self.assertFalse(cleanup_verified(changed, *args))
                for value in mismatches:
                    with self.subTest(row=record["action"], field=key, value=value):
                        changed = deepcopy(records)
                        changed[index][key] = value
                        self.assertFalse(cleanup_verified(changed, *args))
        for index, key, value in (
            (0, "operation", "begin"), (1, "action", "failed"), (2, "action", "not-owned"),
            (2, "running", True),
            (2, "sha", "b" * 40), (2, "deploymentId", "other"),
        ):
            with self.subTest(key=key, value=value):
                changed = deepcopy(records)
                changed[index][key] = value
                self.assertFalse(cleanup_verified(changed, *args))
        for key, value in (("phase", "active"), ("expiresAt", 123457),
                           ("runAttempt", True), ("runId", "124"), ("environment", "production")):
            changed = deepcopy(records)
            changed[2]["use"][key] = value
            self.assertFalse(cleanup_verified(changed, *args))
        for value in (None, dict(use, runId="999"), dict(use, acquiredAt=-1),
                      {key: value for key, value in use.items() if key != "acquiredAt"}):
            changed = deepcopy(records)
            changed[1]["use"] = value
            self.assertFalse(cleanup_verified(changed, *args))
        for index in (1, 2):
            changed = deepcopy(records)
            changed[index]["use"]["acquiredAt"] = -1
            self.assertFalse(cleanup_verified(changed, *args))
        # Recovery can stop the container without proving continuity of the grant epoch.
        recovered = deepcopy(records)
        recovered[1]["epoch"] = recovered[2]["epoch"] = 2
        self.assertFalse(cleanup_verified(recovered, *args))
        self.assertFalse(cleanup_verified(records[:-1], *args))
        self.assertFalse(cleanup_verified(records + [records[-1]], *args))
        self.assertFalse(cleanup_verified(None, *args))

    def test_receipt_reader_refuses_unsafe_or_invalid_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "receipt.jsonl"
            rows = [{"a": 1}, {"b": 2}, {"c": 3}]
            path.write_text("\n".join(map(json.dumps, rows)) + "\n", encoding="utf-8")
            self.assertEqual(read_cleanup_receipt(path), rows)
            for data in (b'', b'{}\n{}\n', b'{"a":1,"a":2}\n{}\n{}',
                         b'{"a":NaN}\n{}\n{}', b'\xff\n{}\n{}', b'x' * 65537):
                path.write_bytes(data)
                self.assertIsNone(read_cleanup_receipt(path))
            link = Path(directory) / "link"
            link.symlink_to(path)
            self.assertIsNone(read_cleanup_receipt(link))
            self.assertIsNone(read_cleanup_receipt(Path(directory) / "missing"))
            self.assertIsNone(read_cleanup_receipt(Path(directory)))


if __name__ == "__main__":
    unittest.main()

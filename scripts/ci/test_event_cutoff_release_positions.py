"""Exercise the offline verifier with synthetic pins and runner output."""

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


VERIFIER = (
    Path(__file__).resolve().parents[2]
    / "tests" / "event_write_cutoff" / "verify_release_positions.py"
)
PINS = {
    "current": {"image": "registry.example/bot@sha256:" + "a" * 64, "source_revision": "1" * 40},
    "retained": {"image": "registry.example/bot@sha256:" + "b" * 64, "source_revision": "2" * 40},
}


def records_for(pins):
    return [
        {
            "images": {position: pin["image"] for position, pin in pins.items()},
            "source_revisions": {position: pin["source_revision"] for position, pin in pins.items()},
        },
        "policy=closed",
        {"startup_refusal": "rehearsal_token_not_synthetic", "verdict": "passed"},
        {"event_and_role_verdict": "passed"},
        {"runtime": "current", "case": "positive", "writes": 1, "verdict": "passed"},
        {"runtime": "current", "case": "wrong-ca", "writes": 0, "verdict": "passed"},
        {"runtime": "retained", "case": "positive", "writes": 1, "verdict": "passed"},
        {"verdict": "passed"},
    ]


class EventCutoffReleasePositionsTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.pins = copy.deepcopy(PINS)
        self.records = records_for(PINS)

    def run_verifier(self, exit_code=0):
        pins = self.root / "pins.json"
        transcript = self.root / "cutoff.log"
        pins.write_text(json.dumps(self.pins), encoding="utf-8")
        transcript.write_text(
            "".join((row if isinstance(row, str) else json.dumps(row)) + "\n" for row in self.records),
            encoding="utf-8",
        )
        return subprocess.run(
            [sys.executable, str(VERIFIER), "--pins", str(pins), "--transcript", str(transcript),
             "--runner-exit-code", str(exit_code)],
            capture_output=True, text=True, check=False, timeout=10,
        )

    def assert_refused(self, reason, exit_code=0):
        result = self.run_verifier(exit_code)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn(reason, result.stderr)

    def test_complete_run_matches_selected_positions(self):
        result = self.run_verifier()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("matches both selected Bot positions", result.stdout)

    def test_swapped_positions_refuse(self):
        self.records = records_for({"current": PINS["retained"], "retained": PINS["current"]})
        self.assert_refused("current image differs from selected pin")

    def test_substituted_image_or_revision_refuses(self):
        for recorded_field, field, replacement in (
            ("images", "image", "registry.example/bot@sha256:" + "c" * 64),
            ("source_revisions", "source_revision", "3" * 40),
        ):
            with self.subTest(field=field):
                self.records = records_for(PINS)
                self.records[0][recorded_field]["retained"] = replacement
                self.assert_refused(f"retained {field} differs from selected pin")

    def test_missing_or_mutable_selected_pins_refuse(self):
        del self.pins["retained"]
        self.assert_refused("must contain current and retained")
        for image in (None, "registry.example/bot:latest", "sha256:" + "a" * 64):
            with self.subTest(image=image):
                self.pins["retained"] = {**PINS["retained"], "image": image}
                self.assert_refused("retained requires an immutable image")
        self.pins["retained"] = {"image": PINS["retained"]["image"]}
        self.assert_refused("retained requires an immutable source_revision")

    def test_missing_reported_artifact_refuses(self):
        del self.records[0]["images"]["retained"]
        self.assert_refused("must report images for both positions")

    def test_duplicate_identity_record_refuses(self):
        self.records.insert(1, self.records[0])
        self.assert_refused("additional rehearsal identity or final record")

    def test_contradictory_duplicate_json_key_refuses(self):
        header = json.dumps(self.records[0])
        self.records[0] = '{"images": {}, ' + header[1:]
        self.assert_refused("Rehearsal identity record contains invalid JSON or duplicate keys")

    def test_partial_stages_cannot_pass(self):
        for missing, reason in (
            (-1, "Missing final rehearsal pass"),
            (3, "event-and-role stage"),
            (-2, "Missing retained announcement positive control"),
        ):
            with self.subTest(missing=missing):
                self.records = records_for(PINS)
                del self.records[missing]
                self.assert_refused(reason)

    def test_success_marker_cannot_override_failed_exit(self):
        self.assert_refused("Rehearsal exited 1", exit_code=1)

    def test_shared_image_still_requires_both_announcement_controls(self):
        self.pins["retained"] = dict(self.pins["current"])
        self.records = records_for(self.pins)
        result = self.run_verifier()
        self.assertEqual(result.returncode, 0, result.stderr)
        del self.records[-2]
        self.assert_refused("Missing retained announcement positive control")


if __name__ == "__main__":
    unittest.main()

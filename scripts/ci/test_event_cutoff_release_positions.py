"""Exercise offline rehearsal preparation and verification with synthetic evidence."""

import copy
from contextlib import redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch


VERIFIER = (
    Path(__file__).resolve().parents[2]
    / "tests" / "event_write_cutoff" / "verify_release_positions.py"
)
RUNBOOK = VERIFIER.parents[2] / "docs" / "internal-event-executors.md"
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


def load_fixture(name):
    spec = importlib.util.spec_from_file_location(name, VERIFIER.with_name(f"{name}.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def capture_runner(pins):
    with patch.dict(sys.modules, recorder=load_fixture("recorder")):
        runner = load_fixture("rehearse")
    by_image = {pin["image"]: pin["source_revision"] for pin in pins.values()}

    def preflight(*argv):
        if argv[:3] == ("docker", "image", "inspect") and len(argv) == 4:
            return json.dumps([{
                "RepoDigests": [argv[3]], "Config": {"Labels": {runner.REVISION: by_image[argv[3]]}},
            }]).encode()
        if argv == ("git", "cat-file", "-e", f'{pins["current"]["source_revision"]}:sql/web_v1.sql'):
            return b""
        raise AssertionError(f"Unexpected rehearsal command {argv}")

    def exercise(_):
        print(json.dumps({"recorder": [], "event_and_role_verdict": "passed"}))

    original_probe = runner.Rehearsal.announcement_control

    def probe(rehearsal, position, case):
        if case != "positive":
            return
        reply = {"status": 200, "message_id": runner.MESSAGE, "replay": False}
        with patch.multiple(
            rehearsal,
            request=Mock(side_effect=[reply, {**reply, "replay": True}]),
            claim=Mock(return_value="completed"),
            journal=Mock(return_value=[("POST", runner.MESSAGE_PATH, "announcement", "applied")]),
        ):
            original_probe(rehearsal, position, case)

    argv = ["rehearse.py", "--current", pins["current"]["image"], "--retained", pins["retained"]["image"]]
    with (
        patch.object(sys, "argv", argv),
        patch.object(runner, "run", side_effect=preflight),
        patch.object(subprocess, "run", side_effect=AssertionError("Rehearsal subprocess disabled")),
        patch.multiple(runner.Rehearsal, start=Mock(), start_bot=Mock(), exercise=exercise,
                       announcement_control=probe),
        redirect_stdout(io.StringIO()) as output,
    ):
        runner.main()
    return output.getvalue()


class EventCutoffReleasePositionsTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.pins = copy.deepcopy(PINS)
        self.records = records_for(PINS)

    def run_verifier(self, exit_code=0, *, capture=None, pipe=False):
        pins = self.root / "pins.json"
        transcript = self.root / "cutoff.log"
        pins.write_text(json.dumps(self.pins), encoding="utf-8")
        if pipe:
            os.mkfifo(transcript)
        else:
            if capture is None:
                capture = "".join(
                    (row if isinstance(row, str) else json.dumps(row)) + "\n" for row in self.records
                )
            transcript.write_text(capture, encoding="utf-8")
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
        result = self.run_verifier(capture=capture_runner(self.pins))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("matches both selected Bot positions", result.stdout)

    def test_non_regular_transcript_refuses_without_waiting_for_a_writer(self):
        result = self.run_verifier(pipe=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn("rehearsal transcript must be a regular file", result.stderr)

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


class EventCutoffPreparationTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        # Rollback appears first in the file to exercise lookup by position.
        self.pins = {
            "retained": {**PINS["retained"], "worker_version": "00000000-0000-4000-8000-000000000002"},
            "current": {**PINS["current"], "worker_version": "00000000-0000-4000-8000-000000000001"},
        }

    def run_preparation(self, raw=None):
        pins = self.root / "selected.json"
        pins.write_text(json.dumps(self.pins) if raw is None else raw, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(VERIFIER.with_name("prepare_rehearsal.py")), "--pins", str(pins)],
            capture_output=True, text=True, check=False, timeout=10,
        )

    def assert_prepared(self, current, retained):
        result = self.run_preparation()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "{} {} {} {}\n".format(
            current["image"], current["source_revision"], retained["image"], retained["source_revision"],
        ))
        self.assertEqual(result.stderr, "")

    def assert_refused(self, reason, raw=None):
        result = self.run_preparation(raw)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, f"Rehearsal preparation refused: {reason}\n")

    def test_preparation_orders_reviewed_positions_by_name(self):
        self.assert_prepared(PINS["current"], PINS["retained"])

    def test_preparation_keeps_both_positions_for_a_shared_image(self):
        self.pins["retained"].update(PINS["current"])
        self.assert_prepared(PINS["current"], PINS["current"])

    def test_preparation_refuses_conflicting_revisions_for_a_shared_image(self):
        for repository in ("registry.example/bot", "mirror.example/bot"):
            with self.subTest(repository=repository):
                self.pins["retained"]["image"] = repository + "@sha256:" + "a" * 64
                self.assert_refused("shared image has conflicting source revisions")

    def test_preparation_refuses_missing_or_invalid_pins(self):
        for position in ("current", "retained"):
            pin = self.pins.pop(position)
            with self.subTest(position=position, missing=position):
                self.assert_refused("Selected pins must contain current and retained positions")
            self.pins[position] = pin
            for field, invalid, requirement in (
                ("image", "registry.example/bot:latest", "an immutable"),
                ("source_revision", "main", "an immutable"),
                ("worker_version", " \t", "a reviewed"),
            ):
                with self.subTest(position=position, field=field):
                    reviewed = pin.pop(field)
                    self.assert_refused(f"{position} requires {requirement} {field}")
                    pin[field] = invalid
                    self.assert_refused(f"{position} requires {requirement} {field}")
                    pin[field] = reviewed

    def test_preparation_refuses_duplicate_keys(self):
        raw = json.dumps(self.pins)
        for duplicated in (
            '{"current": {}, ' + raw[1:],
            raw.replace('"retained": {', '"retained": {"image": null, '),
        ):
            with self.subTest(input=duplicated):
                self.assert_refused("Selected pins contains invalid JSON or duplicate keys", duplicated)

    def test_runbook_checks_both_revision_labels_before_running(self):
        section = RUNBOOK.read_text(encoding="utf-8").split("### Disposable rehearsal\n", 1)[1]
        recipe = section.split("```bash\n", 1)[1].split("```", 1)[0]
        (self.root / "selected.json").write_text(json.dumps(self.pins), encoding="utf-8")
        transcript = self.root / "cutoff.log"
        mocks = r'''
fixture_current=$1 fixture_retained=$2 label_current=$3 label_retained=$4
fixture_python=$5 fixture_prepare=$6
docker() {
  case "$1:$2" in
    image:inspect)
      case "$5" in
        "$fixture_current") printf '%s\n' "$label_current" ;;
        "$fixture_retained") printf '%s\n' "$label_retained" ;;
        *) return 8 ;;
      esac ;;
    pull:*) return 0 ;;
    *) return 9 ;;
  esac
}
git() { [[ "$1" == cat-file && "$2" == -e && "$3" == "$label_current"* ]]; }
python3() {
  case "$1" in
    tests/event_write_cutoff/prepare_rehearsal.py) "$fixture_python" "$fixture_prepare" "${@:2}" ;;
    tests/event_write_cutoff/rehearse.py) printf '%s\n' "${@:2}"; return 7 ;;
    *) return 9 ;;
  esac
}
result=99
'''
        report = '\nrecipe_status=$?\nprintf "%s %s\\n" "$recipe_status" "${result-unset}"\n'
        current, retained = PINS["current"], PINS["retained"]
        for mismatch in (None, "current", "retained"):
            with self.subTest(mismatch=mismatch):
                transcript.write_text("previous rehearsal\n", encoding="utf-8")
                result = subprocess.run(
                    ["bash", "-c", mocks + recipe + report, "rehearsal-fixture",
                     current["image"], retained["image"],
                     "3" * 40 if mismatch == "current" else current["source_revision"],
                     "3" * 40 if mismatch == "retained" else retained["source_revision"],
                     sys.executable, str(VERIFIER.with_name("prepare_rehearsal.py"))],
                    cwd=self.root, capture_output=True, text=True, check=True, timeout=10,
                )
                captured = transcript.read_text(encoding="utf-8").splitlines()
                if mismatch is None:
                    self.assertEqual(result.stdout, "0 7\n")
                    self.assertEqual(result.stderr, "")
                    self.assertEqual(captured, ["--current", current["image"], "--retained", retained["image"]])
                else:
                    self.assertEqual(result.stdout, "1 unset\n")
                    self.assertEqual(result.stderr, "Local image revisions differ from the reviewed pair\n")
                    self.assertEqual(captured, ["previous rehearsal"])


if __name__ == "__main__":
    unittest.main()

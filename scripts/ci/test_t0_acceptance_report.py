"""Offline CLI fixtures for the optional operator handoff report."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "t0_acceptance_report.py"
SHA = "a" * 40
WINDOW = {
    "environment": "staging",
    "build_revision": SHA,
    "build_id": "900-1",
    "deploy_id": "2026.11.25.1+aaaaaaa",
    "image_digest": "sha256:" + "b" * 64,
    "worker_version": "11111111-1111-4111-8111-111111111111",
    "application_id": "22222222-2222-4222-8222-222222222222",
    "start": "2026-11-25T02:00:00Z",
    "end": "2026-11-25T02:05:00Z",
}
HELP = {"schema": "help_embed_smoke.v3", "verdict": "PASS", "rows": [
    {"name": name, "result": "pass", "actual": "read-back branding, text and fields match; message deleted",
     "message_id": str(1000000000000000000 + i), "duration_ms": 10}
    for i, name in enumerate(("help", "help_topic", "help_warn", "help_error", "announcement", "echo",
                              "rsvp_ok", "rsvp_error", "voice_ok", "voice_warn", "voice_error"))
]}
NAMING = {"transport": "offline-naming-observations", "result": "pass", "checks": [
    {"check": name, "verdict": "pass", "detail": "expected name change read back on the same room",
     "command_id": None, "version": None} for name in ("empty-room-rename", "occupied-room-return")
]}
GATEWAY = {"verdict": "observed", "build_sha": SHA, "signals": {
    "ready": 1, "gateway_resumed": 0, "shard_closed": 2, "gateway_reconnect_failed": 1,
    "identify_planned": 1, "resume_planned": 0,
}}


class T0AcceptanceReportTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.paths = {}

    def save(self, kind, source, window=WINDOW):
        path = self.root / f"{kind}.json"
        raw = json.dumps(source).encode()
        path.write_bytes(raw)
        self.paths[kind] = path
        return {"window": window, "sha256": hashlib.sha256(raw).hexdigest()}

    def run_report(self, bindings, expected=WINDOW):
        handoff = self.root / "handoff.json"
        handoff.write_text(json.dumps({"expected": expected, "observations": bindings}), encoding="utf-8")
        command = [sys.executable, "-B", str(SCRIPT), "--handoff", str(handoff)]
        for kind, path in self.paths.items():
            command.extend((f"--{kind}-report", str(path)))
        result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=10)
        self.assertEqual(result.stderr, "")
        self.assertNotIn("1000000000000000000", result.stdout)
        return json.loads(result.stdout)["observations"]

    def test_reads_outputs_from_the_offline_naming_and_gateway_producers(self):
        readbacks = self.root / "readbacks.json"
        readbacks.write_text(json.dumps({"occupied_name": "Fixture owner", "empty_name": "Open seat",
                                        "readbacks": [
            {"room_id": "3000000000000000001", "observed_at_ms": i, "humans": humans, "name": name}
            for i, humans, name in [(1000, 2, "Fixture owner"), (2000, 0, "Open seat"), (3000, 1, "Fixture owner")]
        ]}), encoding="utf-8")
        naming = self.root / "naming.json"
        subprocess.run([sys.executable, "-B", str(SCRIPT.with_name("staging_voice_smoke.py")),
                        "--naming-observations", str(readbacks), "--evidence", str(naming)],
                       capture_output=True, check=True, timeout=10)

        packet = {"schema_version": 1, "service": WINDOW["application_id"], "contains": False,
                  "window": {key: WINDOW[key].replace("Z", ".000Z") for key in ("start", "end")},
                  "event_count": 1, "limit": 100, "possibly_truncated": False,
                  "events": [{"ts": "2026-11-25T02:01:00.000Z", "msg": "ready",
                              "target": "two_bot::gateway", "build_sha": SHA}]}
        result = subprocess.run([sys.executable, "-B", str(SCRIPT.with_name("passive_gateway_evidence.py")),
                                 "--service", WINDOW["application_id"], "--sha", SHA,
                                 "--start", WINDOW["start"], "--end", WINDOW["end"]],
                                input=json.dumps(packet).encode(), capture_output=True, check=True, timeout=10)
        gateway = self.root / "gateway.json"
        gateway.write_bytes(result.stdout)
        self.paths.update(naming=naming, gateway=gateway)
        bindings = {kind: {"window": WINDOW, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                    for kind, path in self.paths.items()}
        rows = self.run_report(bindings)
        self.assertEqual(rows["naming"]["status"], "attributed")
        self.assertEqual(rows["naming"]["result"], "transitions_recorded")
        self.assertEqual(rows["gateway"]["status"], "attributed")
        self.assertEqual(rows["gateway"]["readiness"], "observed")
        self.assertEqual(rows["gateway"]["signals"]["ready"], 1)

    def test_same_sha_builds_and_windows_do_not_mix(self):
        help_binding = self.save("help", HELP)
        other_build = dict(WINDOW, build_id="901-1", deploy_id="2026.11.25.2+aaaaaaa",
                           image_digest="sha256:" + "c" * 64)
        gateway_binding = self.save("gateway", GATEWAY, other_build)
        other_window = dict(WINDOW, start="2026-11-25T02:05:00Z", end="2026-11-25T02:10:00Z")
        naming_binding = self.save("naming", NAMING, other_window)

        bindings = {"help": help_binding, "naming": naming_binding, "gateway": gateway_binding}
        rows = self.run_report(bindings)
        self.assertEqual(rows["help"]["status"], "attributed")
        self.assertEqual(rows["gateway"]["status"], "mismatched")
        self.assertEqual(rows["gateway"]["fields"], ["build_id", "deploy_id", "image_digest"])
        self.assertEqual(rows["naming"]["status"], "mismatched")
        self.assertEqual(rows["naming"]["fields"], ["start", "end"])
        self.assertEqual(rows["gateway"]["window"]["build_revision"], SHA)
        self.assertNotIn("signals", rows["gateway"])

        rows = self.run_report(bindings, other_build)
        self.assertEqual(rows["help"]["status"], "mismatched")
        self.assertEqual(rows["gateway"]["status"], "attributed")
        self.assertEqual(rows["gateway"]["signals"], GATEWAY["signals"])
        self.assertEqual(rows["gateway"]["endpoint_denial"], "unverified")
        self.assertEqual(self.run_report(bindings, other_window)["naming"]["status"], "attributed")

    def test_utc_spellings_preserve_fractional_window_differences(self):
        expected = dict(WINDOW, start="2026-11-25T02:00:00.125Z")
        equivalent = dict(WINDOW, start="2026-11-25T02:00:00.1250+00:00",
                          end="2026-11-25T02:05:00-0000")
        binding = self.save("gateway", GATEWAY, equivalent)
        row = self.run_report({"gateway": binding}, expected)["gateway"]
        self.assertEqual(row["status"], "attributed")
        self.assertEqual(row["window"], expected)
        binding["window"] = dict(equivalent, start="2026-11-25T02:00:00.125000001+0000")
        self.assertEqual(self.run_report({"gateway": binding}, expected)["gateway"]["fields"], ["start"])
        for bad in ("2026-11-25T02:00:00", "2026-11-25T02:00:00+00:30", "2026-02-30T02:00:00Z"):
            with self.subTest(bad=bad):
                binding["window"] = dict(equivalent, start=bad)
                self.assertEqual(self.run_report({"gateway": binding}, expected)["gateway"]["status"],
                                 "unverified")

    def test_help_failure_and_refusal_need_producer_shape(self):
        preflight = {"name": "preflight", "result": "fail", "actual": "synthetic diagnostic", "duration_ms": 0}
        failure = {"schema": HELP["schema"], "verdict": "FAIL", "rows": [preflight]}
        refusal = {"schema": HELP["schema"], "verdict": "REFUSED", "rows": [], "refusal": "synthetic refusal"}
        for report, status in ((failure, "attributed"), (refusal, "attributed"),
                               (dict(failure, rows=[]), "unverified"),
                               (dict(failure, rows=[{}]), "unverified"),
                               (dict(refusal, rows=None), "unverified"),
                               (dict(refusal, refusal=""), "unverified")):
            with self.subTest(report=report):
                rows = self.run_report({"help": self.save("help", report)})
                self.assertEqual(rows["help"]["status"], status)
                if status == "attributed":
                    self.assertEqual(rows["help"]["result"], "probe_failed")

    def test_missing_and_unbound_observations(self):
        self.save("help", HELP)
        self.save("gateway", GATEWAY)
        rows = self.run_report({})
        self.assertEqual({kind: row["status"] for kind, row in rows.items()},
                         {"help": "unverified", "naming": "missing", "gateway": "unverified"})
        self.assertIn("occupied, empty before cleanup and occupied-again", rows["naming"]["needed"])
        self.assertNotIn("signals", rows["gateway"])
        self.assertTrue(all("needed" in row and "result" not in row for row in rows.values()))

    def test_replaced_summary_and_native_sha_mismatch(self):
        binding = self.save("gateway", GATEWAY)
        quiet = dict(GATEWAY, verdict="not_observed", signals=dict.fromkeys(GATEWAY["signals"], 0))
        new_binding = self.save("gateway", quiet)
        row = self.run_report({"gateway": binding})["gateway"]
        self.assertEqual(row["status"], "mismatched")
        self.assertIn("bytes", row["reason"])
        row = self.run_report({"gateway": new_binding})["gateway"]
        self.assertEqual(row["result"], "not_observed")
        self.assertIn("needed", row)

        foreign = self.save("gateway", dict(GATEWAY, build_sha="d" * 40))
        row = self.run_report({"gateway": foreign})["gateway"]
        self.assertEqual(row["status"], "mismatched")
        self.assertIn("build revision", row["reason"])

    def test_staging_and_fixture_reports_cannot_cover_production(self):
        expected = dict(WINDOW, environment="production")
        help_binding = self.save("help", HELP, expected)
        naming_binding = self.save("naming", NAMING, expected)
        rows = self.run_report({"help": help_binding, "naming": naming_binding}, expected)
        self.assertEqual(rows["help"]["status"], "unverified")
        self.assertEqual(rows["naming"]["status"], "unverified")

    def test_partial_observations_and_reported_gateway_mismatch(self):
        checks = [NAMING["checks"][0], dict(NAMING["checks"][1], verdict="skip")]
        naming = self.save("naming", dict(NAMING, result="incomplete", checks=checks))
        reconnect = dict.fromkeys(GATEWAY["signals"], 0)
        reconnect["gateway_reconnect_failed"] = 1
        gateway = self.save("gateway", dict(GATEWAY, signals=reconnect))
        rows = self.run_report({"naming": naming, "gateway": gateway})
        self.assertEqual(rows["naming"]["missing_transitions"], ["occupied-room-return"])
        self.assertEqual(rows["naming"]["result"], "not_observed")
        self.assertEqual(rows["gateway"]["readiness"], "not_observed")
        self.assertIn("readiness_needed", rows["gateway"])

        self.save("gateway", {"verdict": "stale_evidence"})
        row = self.run_report({})["gateway"]
        self.assertEqual(row["status"], "mismatched")
        self.assertEqual(row["source_verdict"], "stale_evidence")

    def test_ambiguous_input_is_unverified_without_echo(self):
        self.save("help", HELP)
        self.paths["help"].write_text('{"verdict":"secret sentinel","verdict":"PASS"}', encoding="utf-8")
        rows = self.run_report({})
        self.assertEqual(rows["help"]["status"], "unverified")
        self.assertNotIn("secret sentinel", json.dumps(rows))


if __name__ == "__main__":
    unittest.main()

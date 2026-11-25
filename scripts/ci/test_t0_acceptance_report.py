"""Offline CLI fixtures for the optional operator handoff report."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
SHA = "a" * 40
WINDOW = {
    "environment": "staging", "build_revision": SHA,
    "build_id": "900-1", "deploy_id": "2026.11.25.1+aaaaaaa",
    "image_digest": "sha256:" + "b" * 64,
    "worker_version": "11111111-1111-4111-8111-111111111111",
    "application_id": "22222222-2222-4222-8222-222222222222",
    "start": "2026-11-25T02:00:00Z", "end": "2026-11-25T02:05:00Z",
}
PRODUCTION = dict(WINDOW, environment="production")
HELP = {"schema": "help_embed_smoke.v3", "verdict": "PASS", "rows": [
    {"name": name, "result": "pass", "actual": "read-back branding, text and fields match; message deleted",
     "message_id": str(1000000000000000000 + i), "duration_ms": 10}
    for i, name in enumerate(("help", "help_topic", "help_warn", "help_error", "announcement", "echo",
                              "rsvp_ok", "rsvp_error", "voice_ok", "voice_warn", "voice_error"))
]}
NAMING = {"transport": "offline-naming-observations", "result": "pass", "checks": [
    {"check": name, "verdict": "pass"} for name in ("empty-room-rename", "occupied-room-return")
]}
GATEWAY = {"verdict": "observed", "build_sha": SHA, "signals": {
    "ready": 1, "gateway_resumed": 0, "shard_closed": 2,
    "gateway_reconnect_failed": 1, "identify_planned": 1, "resume_planned": 0}}


def watch_rows(signal, *observations):
    return {"schema": "production_watch.v1", "signal": signal, "rows": [
        {"utc": f"2026-11-25T02:0{index}:00Z", "observation": check, "disposition": disposition}
        for index, (check, disposition) in enumerate(observations, start=1)
    ]}


class T0AcceptanceReportTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.paths = {}

    def save(self, kind, source, window=WINDOW):
        path = self.root / f"{kind}.json"
        path.write_bytes(json.dumps(source).encode() if isinstance(source, dict) else source)
        self.paths[kind] = path
        return {"window": window, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}

    def invoke(self, script, *args, stdin=None):
        result = subprocess.run([sys.executable, "-B", str(SCRIPTS / script), *args], input=stdin,
                                capture_output=True, check=True, timeout=10)
        self.assertEqual(result.stderr, b"")
        return result.stdout

    def run_report(self, bindings, expected=WINDOW):
        handoff = self.root / "handoff.json"
        handoff.write_text(json.dumps({"expected": expected, "observations": bindings}), encoding="utf-8")
        args = ["--handoff", str(handoff)]
        for kind, path in self.paths.items():
            args.extend((f"--{kind}-report", str(path)))
        raw = self.invoke("t0_acceptance_report.py", *args)
        self.assertNotIn(b"1000000000000000000", raw)
        return json.loads(raw)["observations"]

    def test_reads_outputs_from_the_offline_naming_and_gateway_producers(self):
        readbacks = self.root / "readbacks.json"
        slots = [{"room_id": "3000000000000000001", "observed_at_ms": i, "humans": humans, "name": name}
                 for i, humans, name in [(1000, 2, "Owner"), (2000, 0, "Empty"), (3000, 1, "Owner")]]
        readbacks.write_text(json.dumps({"occupied_name": "Owner", "empty_name": "Empty", "readbacks": slots}))
        naming = self.root / "naming.json"
        self.invoke("staging_voice_smoke.py", "--naming-observations", str(readbacks), "--evidence", str(naming))
        packet = {"schema_version": 1, "service": WINDOW["application_id"], "contains": False,
                  "window": {key: WINDOW[key].replace("Z", ".000Z") for key in ("start", "end")},
                  "event_count": 1, "limit": 100, "possibly_truncated": False,
                  "events": [{"ts": "2026-11-25T02:01:00.000Z", "msg": "ready",
                              "target": "two_bot::gateway", "build_sha": SHA}]}
        gateway = self.invoke("passive_gateway_evidence.py", "--service", WINDOW["application_id"], "--sha", SHA,
                              "--start", WINDOW["start"], "--end", WINDOW["end"], stdin=json.dumps(packet).encode())
        rows = self.run_report({"naming": self.save("naming", naming.read_bytes()),
                                "gateway": self.save("gateway", gateway)})
        self.assertEqual(rows["naming"]["result"], "transitions_recorded")
        self.assertEqual(rows["gateway"]["readiness"], "observed")
        self.assertEqual(rows["gateway"]["signals"]["ready"], 1)
        self.assertEqual(rows["gateway"]["endpoint_denial"], "unverified")

    def test_same_sha_builds_and_windows_do_not_mix(self):
        other_build = dict(WINDOW, build_id="901-1", deploy_id="2026.11.25.2+aaaaaaa", image_digest="sha256:" + "c" * 64)
        other_window = dict(WINDOW, start="2026-11-25T02:05:00Z", end="2026-11-25T02:10:00Z")
        bindings = {"help": self.save("help", HELP), "naming": self.save("naming", NAMING, other_window),
                    "gateway": self.save("gateway", GATEWAY, other_build)}
        rows = self.run_report(bindings)
        self.assertEqual(rows["help"]["status"], "attributed")
        self.assertEqual((rows["gateway"]["status"], rows["gateway"]["fields"]),
                         ("mismatched", ["build_id", "deploy_id", "image_digest"]))
        self.assertEqual((rows["naming"]["status"], rows["naming"]["fields"]), ("mismatched", ["start", "end"]))
        self.assertNotIn("signals", rows["gateway"])
        rows = self.run_report(bindings, other_build)
        self.assertEqual((rows["help"]["status"], rows["gateway"]["signals"]), ("mismatched", GATEWAY["signals"]))
        self.assertEqual(self.run_report(bindings, other_window)["naming"]["status"], "attributed")

    def test_utc_spellings_preserve_fractional_window_differences(self):
        expected = dict(WINDOW, start="2026-11-25T02:00:00.125Z")
        equivalent = dict(WINDOW, start="2026-11-25T02:00:00.1250+00:00", end="2026-11-25T02:05:00-0000")
        binding = self.save("gateway", GATEWAY, equivalent)
        row = self.run_report({"gateway": binding}, expected)["gateway"]
        self.assertEqual((row["status"], row["window"]), ("attributed", expected))
        binding["window"] = dict(equivalent, start="2026-11-25T02:00:00.125000001+0000")
        self.assertEqual(self.run_report({"gateway": binding}, expected)["gateway"]["fields"], ["start"])
        for bad in ("2026-11-25T02:00:00", "2026-11-25T02:00:00+00:30", "2026-02-30T02:00:00Z"):
            binding["window"] = dict(equivalent, start=bad)
            row = self.run_report({"gateway": binding}, expected)["gateway"]
            self.assertEqual(row["status"], "unverified", bad)

    def test_help_failure_and_refusal_need_producer_shape(self):
        preflight = {"name": "preflight", "result": "fail", "actual": "synthetic diagnostic", "duration_ms": 0}
        failure = {"schema": HELP["schema"], "verdict": "FAIL", "rows": [preflight]}
        refusal = {"schema": HELP["schema"], "verdict": "REFUSED", "rows": [], "refusal": "synthetic refusal"}
        embed_failure = dict(failure, rows=[dict(preflight, name="help", cleanup_failed=True)])
        for report in (failure, refusal, embed_failure):
            row = self.run_report({"help": self.save("help", report)})["help"]
            self.assertEqual((row["status"], row["result"]), ("attributed", "probe_failed"))
        for report in (dict(failure, rows=[]), dict(failure, rows=[{}]),
                       dict(refusal, rows=None), dict(refusal, refusal="")):
            row = self.run_report({"help": self.save("help", report)})["help"]
            self.assertEqual(row["status"], "unverified")

    def test_missing_and_unbound_observations(self):
        self.save("help", HELP)
        self.save("gateway", GATEWAY)
        rows = self.run_report({})
        self.assertEqual({kind: row["status"] for kind, row in rows.items()},
                         {"help": "unverified", "naming": "missing", "gateway": "unverified"})
        self.assertNotIn("signals", rows["gateway"])
        self.assertTrue(all("needed" in row and "result" not in row for row in rows.values()))

    def test_replaced_summary_and_native_sha_mismatch(self):
        binding = self.save("gateway", GATEWAY)
        quiet = dict(GATEWAY, verdict="not_observed", signals=dict.fromkeys(GATEWAY["signals"], 0))
        new_binding = self.save("gateway", quiet)
        row = self.run_report({"gateway": binding})["gateway"]
        self.assertEqual((row["status"], row["reason"]),
                         ("mismatched", "saved observation differs from the attributed bytes"))
        row = self.run_report({"gateway": new_binding})["gateway"]
        self.assertEqual(row["result"], "not_observed")
        foreign = self.save("gateway", dict(GATEWAY, build_sha="d" * 40))
        row = self.run_report({"gateway": foreign})["gateway"]
        self.assertEqual(row["status"], "mismatched")
        self.assertIn("build revision", row["reason"])

    def test_staging_and_fixture_reports_cannot_cover_production(self):
        expected = dict(WINDOW, environment="production")
        bindings = {"help": self.save("help", HELP, expected), "naming": self.save("naming", NAMING, expected)}
        rows = self.run_report(bindings, expected)
        self.assertTrue(all(rows[kind]["status"] == "unverified" for kind in bindings))

    def test_production_watch_results_do_not_change_attribution(self):
        gateway = dict(GATEWAY, signals=dict.fromkeys(GATEWAY["signals"], 0) | {"identify_planned": 1})
        for disposition, result in (("pass", "readbacks_and_deletions_recorded"),
                                    ("fail", "probe_failed"), ("not_observed", "not_observed")):
            with self.subTest(disposition=disposition):
                help_report = watch_rows("help", ("branding-readbacks-and-deletions", disposition))
                naming = watch_rows("naming", ("empty-room-rename", "pass"), ("occupied-room-return", disposition))
                rows = self.run_report({"help": self.save("help", help_report, PRODUCTION),
                                        "naming": self.save("naming", naming, PRODUCTION),
                                        "gateway": self.save("gateway", gateway, PRODUCTION)}, PRODUCTION)
                self.assertEqual((rows["help"]["status"], rows["help"]["result"]), ("attributed", result))
                naming_result = "transitions_recorded" if disposition == "pass" else result
                self.assertEqual((rows["naming"]["status"], rows["naming"]["result"]), ("attributed", naming_result))
                self.assertEqual((rows["gateway"]["status"], rows["gateway"]["readiness"],
                                  rows["gateway"]["endpoint_denial"]), ("attributed", "not_observed", "unverified"))

    def test_production_watch_requires_in_window_sanitized_rows_and_exact_binding(self):
        naming = watch_rows("naming", ("empty-room-rename", "pass"))
        self.assertEqual(self.run_report({}, PRODUCTION)["naming"]["status"], "missing")
        binding = self.save("naming", naming, PRODUCTION)
        self.assertEqual(self.run_report({}, PRODUCTION)["naming"]["status"], "unverified")
        observed = self.run_report({"naming": binding}, PRODUCTION)["naming"]
        self.assertEqual((observed["status"], observed["result"], observed["missing_transitions"]),
                         ("attributed", "not_observed", ["occupied-room-return"]))
        staging = self.run_report({"naming": self.save("naming", naming)}, WINDOW)["naming"]
        self.assertEqual(staging["status"], "unverified")

        for change in ({"build_id": "901-1"}, {"end": "2026-11-25T02:04:00Z"}):
            row = self.run_report({"naming": binding}, PRODUCTION | change)["naming"]
            self.assertEqual((row["status"], row["fields"]), ("mismatched", list(change)))
            self.assertNotIn("result", row)

        entry = naming["rows"][0]
        stale = dict(naming, rows=[dict(entry, utc="2026-11-25T02:05:00.000000001Z")])
        changed = self.save("naming", stale, PRODUCTION)
        self.assertEqual(self.run_report({"naming": binding}, PRODUCTION)["naming"]["reason"],
                         "saved observation differs from the attributed bytes")
        self.assertEqual(self.run_report({"naming": changed}, PRODUCTION)["naming"]["reason"],
                         "production watch row falls outside the serving window")

        same_time = dict(entry, observation="occupied-room-return")
        for invalid in (dict(naming, signal="help"), dict(naming, rows=[]), dict(naming, rows=[entry, entry]),
                        dict(naming, rows=[entry, same_time]),
                        dict(naming, rows=[dict(entry, note="synthetic private value")])):
            row = self.run_report({"naming": self.save("naming", invalid, PRODUCTION)}, PRODUCTION)["naming"]
            self.assertEqual(row["status"], "unverified")
            self.assertNotIn("synthetic private value", json.dumps(row))

    def test_production_naming_failure_also_reports_missing_readback(self):
        failure = ("empty-room-rename", "fail")
        for other in ((), (("occupied-room-return", "not_observed"),)):
            with self.subTest(other=other):
                naming = watch_rows("naming", failure, *other)
                row = self.run_report({"naming": self.save("naming", naming, PRODUCTION)}, PRODUCTION)["naming"]
                self.assertEqual((row["status"], row["result"], row["missing_transitions"]),
                                 ("attributed", "probe_failed", ["occupied-room-return"]))

    def test_partial_observations_and_reported_gateway_mismatch(self):
        checks = [NAMING["checks"][0], dict(NAMING["checks"][1], verdict="skip")]
        naming = self.save("naming", dict(NAMING, result="incomplete", checks=checks))
        reconnect = dict(dict.fromkeys(GATEWAY["signals"], 0), gateway_reconnect_failed=1)
        gateway = self.save("gateway", dict(GATEWAY, signals=reconnect))
        rows = self.run_report({"naming": naming, "gateway": gateway})
        self.assertEqual((rows["naming"]["result"], rows["naming"]["missing_transitions"]),
                         ("not_observed", ["occupied-room-return"]))
        self.assertEqual(rows["gateway"]["readiness"], "not_observed")
        self.assertIn("readiness_needed", rows["gateway"])
        self.save("gateway", {"verdict": "stale_evidence"})
        row = self.run_report({})["gateway"]
        self.assertEqual((row["status"], row["source_verdict"]), ("mismatched", "stale_evidence"))

    def test_ambiguous_input_is_unverified_without_echo(self):
        self.save("help", b'{"verdict":"secret sentinel","verdict":"PASS"}')
        rows = self.run_report({})
        self.assertEqual(rows["help"]["status"], "unverified")
        self.assertNotIn("secret sentinel", json.dumps(rows))

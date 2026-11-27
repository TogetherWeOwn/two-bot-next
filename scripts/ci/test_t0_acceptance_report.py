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
WATCH_HEADER = b"utc\tsignal\tobservation\tdisposition\n"
WATCH_HELP = b"2026-11-25T02:01:00Z\thelp\tbranding-readbacks-and-deletions\tpass\n"
WATCH_EMPTY = b"2026-11-25T02:02:00Z\tnaming\tempty-room-rename\tpass\n"
WATCH_RETURN = b"2026-11-25T02:03:00.125+00:00\tnaming\toccupied-room-return\tpass\n"
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
WINDOWED_GATEWAY = dict(GATEWAY, schema="passive_gateway_summary.v1",
                        window={key: WINDOW[key].replace("Z", ".000Z") for key in ("start", "end")})


def watch_rows(signal, *observations):
    return {"schema": "production_watch.v2", "signal": signal, "window": PRODUCTION, "rows": [
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

    def assemble_watch(self, signal, rows, window=PRODUCTION):
        source = self.root / "watch.tsv"
        source.write_bytes(WATCH_HEADER + rows)
        window_path = self.root / "window.json"
        window_path.write_bytes(json.dumps(window).encode() if isinstance(window, dict) else window)
        return subprocess.run(
            [sys.executable, "-B", str(SCRIPTS / "assemble_production_watch.py"),
             "--signal", signal, "--rows", str(source), "--window", str(window_path)],
            capture_output=True, timeout=10,
        )

    def test_assembled_watch_signals_round_trip_with_independent_binding(self):
        equivalent_window = dict(PRODUCTION, start="2026-11-25T02:00:00.000000000+00:00",
                                 end="2026-11-25T02:05:00+0000")
        bindings = {}
        for signal, source in (("help", WATCH_HELP), ("naming", WATCH_EMPTY + WATCH_RETURN)):
            assembled = self.assemble_watch(signal, source, equivalent_window)
            self.assertEqual((assembled.returncode, assembled.stderr), (0, b""))
            self.assertEqual(self.assemble_watch(signal, source).stdout, assembled.stdout)
            receipt = json.loads(assembled.stdout)
            self.assertEqual(receipt.keys(), {"schema", "signal", "window", "rows"})
            self.assertEqual(receipt["window"], PRODUCTION)
            bindings[signal] = self.save(signal, assembled.stdout, PRODUCTION)
        naming = json.loads(self.paths["naming"].read_bytes())
        self.assertEqual(naming["rows"][1]["utc"], "2026-11-25T02:03:00.125+00:00")

        rows = self.run_report({}, PRODUCTION)
        self.assertEqual([rows[signal]["status"] for signal in bindings], ["unverified", "unverified"])
        rows = self.run_report(bindings, PRODUCTION)
        self.assertEqual((rows["help"]["status"], rows["help"]["result"]),
                         ("attributed", "readbacks_and_deletions_recorded"))
        self.assertEqual((rows["naming"]["status"], rows["naming"]["result"]),
                         ("attributed", "transitions_recorded"))
        unavailable = self.assemble_watch("naming", WATCH_EMPTY + WATCH_RETURN.replace(b"pass", b"not_observed"))
        self.assertEqual((unavailable.returncode, unavailable.stderr), (0, b""))
        bindings["naming"] = self.save("naming", unavailable.stdout, PRODUCTION)
        row = self.run_report(bindings, PRODUCTION)["naming"]
        self.assertEqual((row["status"], row["result"], row["missing_transitions"]),
                         ("attributed", "not_observed", ["occupied-room-return"]))

    def test_assembled_watch_refuses_reattribution_to_overlapping_window(self):
        bindings = {}
        for signal, source in (("help", WATCH_HELP), ("naming", WATCH_EMPTY + WATCH_RETURN)):
            assembled = self.assemble_watch(signal, source)
            self.assertEqual((assembled.returncode, assembled.stderr), (0, b""))
            bindings[signal] = self.save(signal, assembled.stdout, PRODUCTION)
        same = self.run_report(bindings, PRODUCTION)
        self.assertEqual((same["help"]["status"], same["help"]["result"]),
                         ("attributed", "readbacks_and_deletions_recorded"))
        self.assertEqual((same["naming"]["status"], same["naming"]["result"]),
                         ("attributed", "transitions_recorded"))

        for change in ({"start": "2026-11-25T02:00:00.000000001Z"},
                       {"end": "2026-11-25T02:04:00Z"}, {"build_id": "901-1"}):
            other = PRODUCTION | change
            # The independent handoff agrees with itself, and all saved bytes and timestamps still fit.
            repackaged = {signal: dict(binding, window=other) for signal, binding in bindings.items()}
            rows = self.run_report(repackaged, other)
            for signal in bindings:
                with self.subTest(signal=signal, change=change):
                    row = rows[signal]
                    self.assertEqual(row["status"], "mismatched")
                    self.assertEqual(row["fields"], list(change))
                    self.assertNotIn("result", row)

    def test_production_watch_requires_a_supported_receipt_with_a_complete_production_window(self):
        sentinel = "synthetic private value"
        for signal, checks in (("help", (("branding-readbacks-and-deletions", "pass"),)),
                               ("naming", (("empty-room-rename", "pass"), ("occupied-room-return", "pass")))):
            report = watch_rows(signal, *checks)
            missing = {key: value for key, value in report.items() if key != "window"}
            legacy = dict(missing, schema="production_watch.v1")
            for invalid in (legacy, missing, dict(report, schema="production_watch.v3"),
                            dict(report, window=WINDOW), dict(report, window=dict(PRODUCTION, end=None)),
                            dict(report, window=dict(PRODUCTION, note=sentinel)), dict(report, note=sentinel)):
                with self.subTest(signal=signal, receipt=invalid):
                    binding = self.save(signal, invalid, PRODUCTION)
                    row = self.run_report({signal: binding}, PRODUCTION)[signal]
                    self.assertEqual(row["status"], "unverified")
                    self.assertNotIn("result", row)
                    self.assertNotIn(sentinel, json.dumps(row))
                    if invalid is legacy:
                        self.assertEqual(row["reason"], "legacy production watch lacks the assembler serving window")

    def test_watch_assembly_refuses_late_incomplete_or_unapproved_input_without_echo(self):
        late = WATCH_RETURN.replace(b"02:03:00.125+00:00", b"02:05:00.000000001Z")
        sentinel = b"synthetic private value"
        duplicate_key = b'{"environment":"' + sentinel + b'",' + json.dumps(PRODUCTION).encode()[1:]
        for index, (signal, source, window) in enumerate((
            ("naming", WATCH_EMPTY + late, PRODUCTION),
            ("naming", WATCH_EMPTY, PRODUCTION),
            ("naming", WATCH_EMPTY + WATCH_EMPTY, PRODUCTION),
            ("naming", WATCH_EMPTY + WATCH_HELP, PRODUCTION),
            ("naming", WATCH_EMPTY.replace(b"02:02:00", b"02:04:00") + WATCH_RETURN, PRODUCTION),
            ("help", WATCH_HELP.replace(b"pass", b"pass\t" + sentinel), PRODUCTION),
            ("help", WATCH_HELP.replace(b"pass", sentinel), PRODUCTION),
            ("help", WATCH_HELP.replace(b"pass", b" pass"), PRODUCTION),
            ("help", WATCH_HELP.replace(b"pass", b"\xff"), PRODUCTION),
            ("help", WATCH_HELP + b"0" * 65536, PRODUCTION),
            ("help", WATCH_HELP, WINDOW),
            ("help", WATCH_HELP, dict(PRODUCTION, end=None)),
            ("help", WATCH_HELP, dict(PRODUCTION, note=sentinel.decode())),
            ("help", WATCH_HELP, duplicate_key),
            (sentinel.decode(), WATCH_HELP, PRODUCTION),
        )):
            with self.subTest(case=index):
                refused = self.assemble_watch(signal, source, window)
                self.assertEqual((refused.returncode, refused.stdout), (2, b""))
                self.assertEqual(refused.stderr, b"production watch refused: missing or invalid rows or serving window\n")

    def test_naming_watch_preserves_exact_same_second_order_and_bounds(self):
        early = "2026-11-25T02:02:00.1234567Z"
        late = "2026-11-25T02:02:00.123456701+0000"
        window = dict(PRODUCTION, start=early, end="2026-11-25T02:02:00.123456701Z")
        equivalent = dict(window, start="2026-11-25T02:02:00.123456700+00:00",
                          end="2026-11-25T02:02:00.1234567010-0000")

        def assemble(first, second):
            empty = WATCH_EMPTY.replace(b"2026-11-25T02:02:00Z", first.encode())
            returned = WATCH_RETURN.replace(b"2026-11-25T02:03:00.125+00:00", second.encode())
            return self.assemble_watch("naming", empty + returned, equivalent)

        assembled = assemble(early, late)
        self.assertEqual((assembled.returncode, assembled.stderr), (0, b""))
        receipt = json.loads(assembled.stdout)
        self.assertEqual(receipt["window"], window)
        self.assertEqual([row["utc"] for row in receipt["rows"]], [early, late])
        binding = self.save("naming", assembled.stdout, equivalent)
        self.assertEqual(self.run_report({"naming": binding}, window)["naming"], {
            "status": "attributed", "window": window, "result": "transitions_recorded", "missing_transitions": [],
        })

        invalid_order = ("unverified", "saved probe output is invalid or refused")
        outside_window = ("mismatched", "production watch row falls outside the serving window")
        for first, second, rejection in (
            (late, early, invalid_order),
            (early, "2026-11-25T02:02:00.123456700-00:00", invalid_order),
            ("2026-11-25T02:02:00.123456699Z", late, outside_window),
            (early, "2026-11-25T02:02:00.123456702Z", outside_window),
        ):
            with self.subTest(first=first, second=second):
                refused = assemble(first, second)
                self.assertEqual((refused.returncode, refused.stdout), (2, b""))
                # Give the reader matching attribution so it must reject the timestamps itself.
                changed = dict(receipt, rows=[dict(row, utc=stamp)
                                             for row, stamp in zip(receipt["rows"], (first, second))])
                binding = self.save("naming", changed, equivalent)
                row = self.run_report({"naming": binding}, window)["naming"]
                self.assertEqual((row["status"], row["reason"]), rejection)
                self.assertNotIn("result", row)

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
        gateway = dict(WINDOWED_GATEWAY, signals=dict.fromkeys(GATEWAY["signals"], 0) | {"identify_planned": 1})
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

    def test_production_gateway_requires_supported_summary_with_capture_bounds(self):
        bounds = WINDOWED_GATEWAY["window"]
        sentinel = "synthetic private value"
        for summary in (GATEWAY, dict(WINDOWED_GATEWAY, schema="passive_gateway_summary.v3"),
                        dict(GATEWAY, window=bounds), dict(WINDOWED_GATEWAY, window={"start": bounds["start"]}),
                        dict(WINDOWED_GATEWAY, window=dict(bounds, service=sentinel)),
                        dict(WINDOWED_GATEWAY, window=dict(bounds, end=sentinel)),
                        dict(WINDOWED_GATEWAY, window=dict(bounds, end=bounds["start"])),
                        dict(WINDOWED_GATEWAY, window=dict(bounds, end="2026-11-25T02:31:00Z")),
                        dict(WINDOWED_GATEWAY, window=dict(bounds, end="2026-11-25T02:05:00.000000001Z"))):
            with self.subTest(summary=summary):
                row = self.run_report({"gateway": self.save("gateway", summary, PRODUCTION)}, PRODUCTION)["gateway"]
                self.assertEqual((row["status"], row["endpoint_denial"]), ("unverified", "unverified"))
                self.assertNotIn(sentinel, json.dumps(row))
                for field in ("result", "signals", "readiness"):
                    self.assertNotIn(field, row)

    def test_endpoint_policy_receipt_needs_an_explicit_exact_window_refusal(self):
        at = "2026-11-25T02:01:00.125Z"
        marker = {"ts": at, "msg": "gateway_resume_endpoint_policy_refused",
                  "target": "two_bot::gateway", "build_sha": SHA}
        opaque = "synthetic private value"

        def summarize(*events):
            packet = {"schema_version": 1, "service": PRODUCTION["application_id"], "contains": False,
                      "window": WINDOWED_GATEWAY["window"], "event_count": len(events),
                      "limit": 100, "possibly_truncated": False, "events": events}
            return self.invoke("passive_gateway_evidence.py", "--service", PRODUCTION["application_id"],
                               "--sha", SHA, "--start", PRODUCTION["start"], "--end", PRODUCTION["end"],
                               stdin=json.dumps(packet).encode())

        receipt = summarize(dict(marker, endpoint=opaque, payload=opaque, token=opaque, member_id=opaque))
        binding = self.save("gateway", receipt, PRODUCTION)
        row = self.run_report({"gateway": binding}, PRODUCTION)["gateway"]
        self.assertEqual((row["status"], row["endpoint_denial"], row["endpoint_denial_at"], row["readiness"]),
                         ("attributed", "observed", at, "not_observed"))
        self.assertNotIn(opaque.encode(), receipt)
        self.assertNotIn(opaque, json.dumps(row))
        self.assertEqual(self.run_report({}, PRODUCTION)["gateway"]["endpoint_denial"], "unverified")
        for change in ({"application_id": WINDOW["worker_version"]},
                       {"image_digest": "sha256:" + "c" * 64}):
            row = self.run_report({"gateway": binding}, PRODUCTION | change)["gateway"]
            self.assertEqual((row["status"], row["endpoint_denial"]), ("mismatched", "unverified"))

        other_bounds = dict(PRODUCTION, start="2026-11-25T02:00:00.000000001Z")
        row = self.run_report({"gateway": dict(binding, window=other_bounds)}, other_bounds)["gateway"]
        self.assertEqual((row["status"], row["fields"], row["endpoint_denial"]),
                         ("mismatched", ["start"], "unverified"))

        parsed = json.loads(receipt)
        for variant, status in (
            (dict(parsed, endpoint_policy_refused_at="2026-11-25T02:05:00.001Z"), "mismatched"),
            (dict(parsed, endpoint_policy_refused_at=opaque), "unverified"),
            ({key: value for key, value in parsed.items() if key != "endpoint_policy_refused_at"}, "unverified"),
            (dict(parsed, schema=WINDOWED_GATEWAY["schema"]), "unverified"),
            (dict(parsed, endpoint=opaque), "unverified"),
        ):
            changed = self.save("gateway", variant, PRODUCTION)
            row = self.run_report({"gateway": binding}, PRODUCTION)["gateway"]
            self.assertEqual((row["status"], row["endpoint_denial"]), ("mismatched", "unverified"))
            row = self.run_report({"gateway": changed}, PRODUCTION)["gateway"]
            self.assertEqual((row["status"], row["endpoint_denial"]), (status, "unverified"))
            self.assertNotIn("endpoint_denial_at", row)
            self.assertNotIn(opaque, json.dumps(row))

        generic = summarize(dict(marker, msg="ready"), dict(marker, msg="gateway_reconnect_failed"),
                            dict(marker, msg="gateway_connecting", target="two_bot", resume=False),
                            dict(marker, msg="shard_closed", target="two_bot::logging"),
                            dict(marker, target="two_bot"))
        row = self.run_report({"gateway": self.save("gateway", generic, PRODUCTION)}, PRODUCTION)["gateway"]
        self.assertEqual((row["status"], row["readiness"], row["endpoint_denial"]),
                         ("attributed", "observed", "unverified"))
        self.assertNotIn("endpoint_denial_at", row)

    def test_production_gateway_compares_each_bound_without_truncation(self):
        alternate = dict(PRODUCTION, start="2026-11-25T02:00:00.000000000+00:00",
                         end="2026-11-25T02:05:00+0000")
        quiet = dict(WINDOWED_GATEWAY, verdict="not_observed", signals=dict.fromkeys(GATEWAY["signals"], 0))
        binding = self.save("gateway", quiet, alternate)
        self.assertEqual(self.run_report({}, PRODUCTION)["gateway"]["status"], "unverified")
        row = self.run_report({"gateway": binding}, PRODUCTION)["gateway"]
        self.assertEqual((row["status"], row["result"], row["readiness"], row["endpoint_denial"]),
                         ("attributed", "not_observed", "not_observed", "unverified"))
        for key in ("start", "end"):
            with self.subTest(bound=key):
                shifted = {**PRODUCTION, key: PRODUCTION[key].replace("Z", ".000000001Z")}
                binding["window"] = shifted
                row = self.run_report({"gateway": binding}, shifted)["gateway"]
                self.assertEqual((row["status"], row["fields"]), ("mismatched", [key]))
                self.assertNotIn("signals", row)

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

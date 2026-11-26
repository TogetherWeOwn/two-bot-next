"""Replay synthetic Help, naming and gateway evidence through one execution card."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from test_t0_acceptance_report import PRODUCTION, SCRIPTS, WATCH_EMPTY, WATCH_HEADER, WATCH_HELP, WATCH_RETURN

sys.path.insert(0, str(SCRIPTS))
from test_staging_help_embed_smoke import CHANNEL, STAGING_GUILD, TOKEN, FakeDiscord, payloads, smoke


class ProductionExecutionCardReplayTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))

    def invoke(self, script, *args, stdin=b""):
        run = subprocess.run(
            [sys.executable, "-B", str(SCRIPTS / script), *args],
            input=stdin, capture_output=True, check=True, env={}, timeout=10,
        )
        self.assertEqual(run.stderr, b"")
        return run.stdout

    def assemble_watch(self, signal, rows, window_path):
        source = self.root / f"{signal}.tsv"
        source.write_bytes(WATCH_HEADER + rows)
        report = self.root / f"{signal}.json"
        report.write_bytes(self.invoke(
            "assemble_production_watch.py", "--signal", signal,
            "--rows", str(source), "--window", str(window_path),
        ))
        return report

    def summarize_gateway(self, window, event, timestamp, filename):
        packet = {
            "schema_version": 1, "service": window["application_id"], "contains": False,
            "window": {key: window[key].replace("Z", ".000Z") for key in ("start", "end")},
            "event_count": 1, "limit": 100, "possibly_truncated": False,
            "events": [{"ts": timestamp, "msg": event, "target": "two_bot::gateway",
                        "build_sha": window["build_revision"]}],
        }
        summary = self.root / filename
        summary.write_bytes(self.invoke(
            "passive_gateway_evidence.py", "--service", window["application_id"],
            "--sha", window["build_revision"], "--start", window["start"], "--end", window["end"],
            stdin=json.dumps(packet).encode(),
        ))
        return summary

    def replay(self, expected, reports):
        handoff = self.root / "handoff.json"
        bindings = {}
        args = ["--handoff", str(handoff)]
        for signal, (path, window) in reports.items():
            bindings[signal] = {"window": window, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            args.extend((f"--{signal}-report", str(path)))
        handoff.write_text(json.dumps({"expected": expected, "observations": bindings}), encoding="utf-8")
        report = json.loads(self.invoke("t0_acceptance_report.py", *args))
        self.assertEqual(report["expected"], expected)
        return report["observations"]

    def test_help_watch_distinguishes_failed_cleanup_from_unavailable_observation(self):
        payload_path = self.root / "help-payloads.json"
        payload_path.write_text(json.dumps(payloads()), encoding="utf-8")
        window_path = self.root / "window.json"
        window_path.write_text(json.dumps(PRODUCTION), encoding="utf-8")
        cases = (
            (204, "pass", "readbacks_and_deletions_recorded"),
            (403, "fail", "probe_failed"),
            (None, "not_observed", "not_observed"),
        )
        for delete_status, expected_disposition, result in cases:
            with self.subTest(disposition=expected_disposition):
                disposition = "not_observed"
                if delete_status is not None:
                    discord = FakeDiscord(delete_status=delete_status)
                    with mock.patch.object(smoke, "make_transport", side_effect=AssertionError("network access")):
                        smoke_report = smoke.run(payload_path, STAGING_GUILD, TOKEN, CHANNEL, transport=discord)
                    receipt = smoke_report.evidence()
                    disposition = receipt["verdict"].lower()
                    self.assertEqual(disposition, expected_disposition)
                    first_row = receipt["rows"][0]
                    self.assertEqual(first_row["name"], "help")
                    message_path = f"/channels/{CHANNEL}/messages/{first_row['message_id']}"
                    self.assertEqual([method for method, path, _ in discord.calls if path == message_path],
                                     ["GET", "DELETE"])
                    if disposition == "fail":
                        self.assertEqual(smoke_report.exit_code(), 3)
                        self.assertEqual(len(receipt["rows"]), 1)
                        self.assertIs(first_row["cleanup_failed"], True)
                        self.assertTrue(first_row["actual"].startswith("cleanup delete failed ("))
                        self.assertEqual(set(discord.messages), {first_row["message_id"]})
                    else:
                        self.assertEqual(smoke_report.exit_code(), 0)
                        self.assertEqual(len(receipt["rows"]), len(smoke.EMBED_KEYS))
                        self.assertFalse(discord.messages)

                # The operator transcribes the approved card row. An unavailable
                # observation has no cleanup verdict to transcribe.
                source = WATCH_HELP.replace(b"\tpass\n", b"\t" + disposition.encode() + b"\n")
                report = self.assemble_watch("help", source, window_path)
                row = self.replay(PRODUCTION, {"help": (report, PRODUCTION)})["help"]
                self.assertEqual((row["status"], row["window"], row["result"]),
                                 ("attributed", PRODUCTION, result))
                self.assertEqual("needed" in row, disposition != "pass")

    def test_rollback_window_rejects_pre_rollback_readiness_with_exact_digest(self):
        # The restored image came from the same source revision. Its serving identity is distinct.
        restored = dict(
            PRODUCTION, build_id="899-1", deploy_id="2026.11.24.1+aaaaaaa",
            image_digest="sha256:" + "c" * 64, worker_version="44444444-4444-4444-8444-444444444444",
            start="2026-11-25T03:00:00Z", end="2026-11-25T03:05:00Z",
        )
        window_path = self.root / "restored-window.json"
        window_path.write_text(json.dumps(restored), encoding="utf-8")
        help_report = self.assemble_watch("help", WATCH_HELP.replace(b"T02:", b"T03:"), window_path)
        naming = self.assemble_watch("naming", (WATCH_EMPTY + WATCH_RETURN).replace(b"T02:", b"T03:"), window_path)
        restored_gateway = self.summarize_gateway(
            restored, "gateway_resumed", "2026-11-25T03:01:00.000Z", "restored-gateway.json",
        )
        reports = {"help": (help_report, restored), "naming": (naming, restored),
                   "gateway": (restored_gateway, restored)}

        coherent = self.replay(restored, reports)
        self.assertEqual({kind: row["status"] for kind, row in coherent.items()},
                         dict.fromkeys(reports, "attributed"))
        for row in coherent.values():
            self.assertEqual(row["window"], restored)
        self.assertEqual(coherent["help"]["result"], "readbacks_and_deletions_recorded")
        self.assertEqual(coherent["naming"]["result"], "transitions_recorded")
        self.assertEqual(coherent["naming"]["missing_transitions"], [])
        self.assertEqual(coherent["gateway"]["readiness"], "observed")
        self.assertEqual(coherent["gateway"]["signals"]["gateway_resumed"], 1)
        self.assertEqual(coherent["gateway"]["endpoint_denial"], "unverified")

        old_gateway = self.summarize_gateway(
            PRODUCTION, "ready", "2026-11-25T02:01:00.000Z", "pre-rollback-gateway.json",
        )
        self.assertEqual(json.loads(old_gateway.read_bytes())["signals"]["ready"], 1)
        mixed = self.replay(restored, reports | {"gateway": (old_gateway, PRODUCTION)})
        self.assertEqual(mixed["help"], coherent["help"])
        self.assertEqual(mixed["naming"], coherent["naming"])
        gateway = mixed["gateway"]
        self.assertEqual(gateway["status"], "mismatched")
        self.assertEqual(gateway["window"], PRODUCTION)
        self.assertEqual(gateway["fields"], ["build_id", "deploy_id", "image_digest", "worker_version", "start", "end"])
        self.assertEqual(gateway["endpoint_denial"], "unverified")
        for field in ("result", "signals", "readiness"):
            self.assertNotIn(field, gateway)

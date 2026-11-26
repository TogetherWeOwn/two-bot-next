"""Replay synthetic Help, naming and gateway evidence through one execution card."""

from copy import deepcopy
from dataclasses import asdict
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock

from test_production_recovery_artifact import producer, release_run_fixture
from test_t0_acceptance_report import PRODUCTION, SCRIPTS, WATCH_EMPTY, WATCH_HEADER, WATCH_HELP, WATCH_RETURN

sys.path.insert(0, str(SCRIPTS))
from production_recovery_artifact import FORWARD_JOB, FORWARD_STEPS, protected_job
from production_recovery_takeover import RecoveryPin
from production_recovery_workflow import fence_actor, restore_under_fence
from staging_rollout import GateError


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
        report = self.replay_report(expected, reports)
        self.assertEqual(set(report), {"expected", "observations"})
        return report["observations"]

    def replay_report(self, expected, reports, **handoff_fields):
        handoff = self.root / "handoff.json"
        bindings = {}
        args = ["--handoff", str(handoff)]
        for signal, (path, window) in reports.items():
            bindings[signal] = {"window": window, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            args.extend((f"--{signal}-report", str(path)))
        handoff.write_text(json.dumps({"expected": expected, "observations": bindings, **handoff_fields}),
                           encoding="utf-8")
        report = json.loads(self.invoke("t0_acceptance_report.py", *args))
        self.assertEqual(report["expected"], expected)
        return report

    def test_serving_change_interrupts_passing_help_and_gateway_window(self):
        window_path = self.root / "window.json"
        window_path.write_text(json.dumps(PRODUCTION), encoding="utf-8")
        help_report = self.assemble_watch("help", WATCH_HELP, window_path)
        naming = self.assemble_watch("naming", WATCH_EMPTY + WATCH_RETURN, window_path)
        gateway = self.summarize_gateway(PRODUCTION, "ready", "2026-11-25T02:04:00.000Z", "gateway.json")
        reports = {"help": (help_report, PRODUCTION), "naming": (naming, PRODUCTION),
                   "gateway": (gateway, PRODUCTION)}

        identity = {key: value for key, value in PRODUCTION.items() if key not in ("start", "end")}
        readbacks = [dict(identity, utc=at) for at in (
            PRODUCTION["start"], "2026-11-25T02:02:00.000000001+00:00", PRODUCTION["end"],
        )]
        same_build = self.replay_report(PRODUCTION, reports, serving_readbacks=readbacks)
        self.assertEqual(same_build, self.replay_report(PRODUCTION, reports))
        help_row, naming_row, gateway_row = (same_build["observations"][kind] for kind in reports)
        self.assertEqual((help_row["status"], help_row["result"]), ("attributed", "readbacks_and_deletions_recorded"))
        self.assertEqual((naming_row["status"], naming_row["result"]), ("attributed", "transitions_recorded"))
        self.assertEqual((gateway_row["status"], gateway_row["readiness"]), ("attributed", "observed"))

        # Receipts, hashes, the expected SHA, and opening and closing readbacks stay fixed.
        for change in (
            {"deploy_id": "2026.11.25.2+aaaaaaa"},
            {"worker_version": "44444444-4444-4444-8444-444444444444"},
            {"build_id": "901-1"},
            {"image_digest": "sha256:" + "c" * 64},
            {"application_id": "55555555-5555-4555-8555-555555555555"},
            {"build_revision": "c" * 40, "deploy_id": "2026.11.25.2+ccccccc"},
        ):
            with self.subTest(change=change):
                interrupted = self.replay_report(PRODUCTION, reports, serving_readbacks=[
                    readbacks[0], readbacks[1] | change, readbacks[2],
                ])
                self.assertEqual(interrupted.get("window_status"), "interrupted")
                for kind in reports:
                    row = interrupted["observations"][kind]
                    self.assertEqual(row["status"], "mismatched")
                    self.assertTrue({"result", "signals", "readiness"}.isdisjoint(row))

        without_naming = {"help": reports["help"], "gateway": reports["gateway"]}
        interrupted = self.replay_report(PRODUCTION, without_naming, serving_readbacks=[
            dict(readbacks[1], deploy_id="2026.11.25.2+aaaaaaa"),
        ])
        self.assertEqual(interrupted["observations"]["naming"]["status"], "missing")

    def test_invalid_serving_readbacks_refuse_the_handoff_without_echo(self):
        handoff = self.root / "invalid.json"
        identity = {key: value for key, value in PRODUCTION.items() if key not in ("start", "end")}
        observed = dict(identity, utc=PRODUCTION["end"])
        for readbacks in (None, [dict(observed, utc="2026-11-25T01:59:59.999999999Z")],
                          [dict(observed, utc="2026-11-25T02:05:00.000000001Z")],
                          [dict(observed, environment="staging")],
                          [dict(observed, deploy_id="2026.11.25.2+aaaaaaa"), dict(observed, note="sentinel")]):
            with self.subTest(readbacks=readbacks):
                handoff.write_text(json.dumps({"expected": PRODUCTION, "serving_readbacks": readbacks}),
                                   encoding="utf-8")
                run = subprocess.run([sys.executable, "-B", str(SCRIPTS / "t0_acceptance_report.py"),
                                      "--handoff", str(handoff)], capture_output=True, env={}, timeout=10)
                self.assertEqual((run.returncode, run.stderr), (2, b""))
                self.assertEqual(json.loads(run.stdout), {"error": "handoff contains invalid serving readbacks"})

    def recover_predecessor(self, pin, candidate, before):
        inverse = {"state": "restored", **asdict(pin)}
        version, sha, digest = before
        serving = {
            "worker": {"version_id": version, "sha": sha, "traffic_percent": 100},
            "container": {"application_id": pin.application_id, "namespace_id": pin.namespace_id,
                          "image_digest": digest, "rollout_id": "00000000-0000-4000-8000-000000000001",
                          "rollout_status": "completed"},
            "ownership": {"phase": "fenced", "running": False, "epoch": pin.fence_epoch,
                          "owner_actor": fence_actor(pin)},
        }

        def takeover(_operation, _epoch, *, expected_deployment):
            self.assertEqual(serving["worker"]["version_id"], expected_deployment)
            self.assertEqual(serving["container"]["image_digest"], pin.image_digest)
            serving["ownership"].update(phase="active", running=True, epoch=pin.fence_epoch + 1,
                                        owner_actor=pin.actor)

        def wait_stable(version, _started):
            self.assertEqual(serving["worker"]["version_id"], version)
            self.assertEqual(serving["ownership"]["phase"], "active")
            self.assertIs(serving["ownership"]["running"], True)

        rollback, control = Mock(), Mock(side_effect=takeover)

        def wait_worker_restore(version):
            # The provider can acknowledge the restore before that version is visible.
            rollback.post_worker_restore.assert_called_once_with(version)
            serving["worker"].update(version_id=version, sha=pin.rollback_sha)

        rollback.wait_worker_restore.side_effect = wait_worker_restore
        rollback.restore_image.side_effect = lambda _image: serving["container"].update(
            image_digest=pin.image_digest, rollout_id="00000000-0000-4000-8000-000000000002",
        )
        rollback.wait_stable.side_effect = wait_stable
        observe = Mock(side_effect=lambda: deepcopy(serving))
        restore_under_fence(pin, inverse, observe, control, rollback, "fixture-image", candidate)
        if before == candidate:
            rollback.post_worker_restore.assert_called_once_with(pin.worker_version)
            rollback.wait_worker_restore.assert_called_once_with(pin.worker_version)
            rollback.restore_image.assert_called_once_with("fixture-image")
        else:
            rollback.post_worker_restore.assert_not_called()
            rollback.wait_worker_restore.assert_not_called()
            rollback.restore_image.assert_not_called()
        control.assert_called_once_with("takeover-fenced", pin.fence_epoch, expected_deployment=pin.worker_version)
        rollback.wait_stable.assert_called_once_with(pin.worker_version, rollback.now.return_value)
        return serving

    def test_recovery_readiness_cannot_certify_the_forward_candidate(self):
        fixture = Path(__file__).with_name("fixtures") / "production_inverse_admission.json"
        case = json.loads(fixture.read_text(encoding="utf-8"))
        admission = case["admission"]
        pin = RecoveryPin(**admission["pin"])
        candidate = (admission["candidate_worker"], pin.candidate_sha, admission["candidate_image"])
        predecessor = (pin.worker_version, pin.rollback_sha, pin.image_digest)
        intended = dict(PRODUCTION, worker_version=candidate[0], build_revision=candidate[1],
                        image_digest=candidate[2], application_id=pin.application_id)
        _, origin = release_run_fixture()
        origin["id"] = 456
        for conclusion, before in (("cancelled", predecessor), ("success", candidate)):
            with self.subTest(forward_job=conclusion):
                # Applying the schema can finish before the run is interrupted at receipt upload.
                job = producer(origin, FORWARD_JOB, FORWARD_STEPS)
                job["conclusion"] = conclusion
                job["steps"][-1]["conclusion"] = conclusion
                api = Mock(return_value={"total_count": 1, "jobs": [job]})
                if conclusion == "cancelled":
                    with self.assertRaisesRegex(GateError, "^production_inverse_job_invalid$"):
                        protected_job(origin, api, FORWARD_JOB, FORWARD_STEPS)
                else:
                    protected_job(origin, api, FORWARD_JOB, FORWARD_STEPS)

                serving = self.recover_predecessor(pin, candidate, before)
                restored = dict(
                    PRODUCTION, build_revision=serving["worker"]["sha"], build_id="899-1",
                    deploy_id="2026.11.24.1+bbbbbbb", image_digest=serving["container"]["image_digest"],
                    worker_version=serving["worker"]["version_id"], application_id=serving["container"]["application_id"],
                )
                window_path = self.root / "recovered-window.json"
                window_path.write_text(json.dumps(restored), encoding="utf-8")
                help_report = self.assemble_watch("help", WATCH_HELP, window_path)
                gateway = self.summarize_gateway(
                    restored, "gateway_resumed", "2026-11-25T02:01:00.000Z", "recovered-gateway.json",
                )
                reports = {"help": (help_report, restored), "gateway": (gateway, restored)}
                recovered = self.replay(restored, reports)
                self.assertEqual((recovered["help"]["status"], recovered["help"]["result"]),
                                 ("attributed", "readbacks_and_deletions_recorded"))
                self.assertEqual((recovered["gateway"]["status"], recovered["gateway"]["readiness"]),
                                 ("attributed", "observed"))

                refused = self.replay(intended, reports)
                self.assertEqual(refused["help"]["fields"],
                                 ["build_revision", "build_id", "deploy_id", "image_digest", "worker_version"])
                self.assertEqual(refused["gateway"]["reason"], "gateway summary names a different build revision")
                for signal in reports:
                    row = refused[signal]
                    self.assertEqual(row["status"], "mismatched")
                    self.assertTrue({"result", "readiness", "signals"}.isdisjoint(row))

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

        # The current label and exact hash can both be correct for the supplied file path.
        # The saved producer output itself still came from the older window.
        mislabeled = self.replay(restored, reports | {"gateway": (old_gateway, restored)})
        self.assertEqual(mislabeled["help"], coherent["help"])
        self.assertEqual(mislabeled["naming"], coherent["naming"])
        gateway = mislabeled["gateway"]
        self.assertEqual(gateway["status"], "mismatched")
        self.assertEqual(gateway["fields"], ["start", "end"])
        self.assertEqual(gateway["endpoint_denial"], "unverified")
        for field in ("result", "signals", "readiness"):
            self.assertNotIn(field, gateway)

"""Exercise the paired readiness contract without a provider or network."""

from dataclasses import replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_readiness import (
        Build, Ledger, MockActivity, Probe, Readback, Reservation, Runtime, Writer, verify_paired_readiness,
    )
    from pre_target import Target, TargetError


def digest(char):
    return "sha256:" + char * 64


BUILD = Build("a" * 40, "123-1", "pre-deploy", digest("b"))
ORIGINAL = Reservation(
    Runtime(Target("pre-fixture-original", 1, 1_800_000_000, "0473/0465"),
            "10000000-0000-0000-0000-000000000001", "process-one"),
    "20000000-0000-0000-0000-000000000001", BUILD, Writer("db-one", "writer-one", True, True, True),
    Ledger(digest("a"), digest("c"), (115, 473), (20, 465), False, None), "ready-one",
)
RETAINED = Reservation(
    Runtime(Target("pre-fixture-retained", 2, 1_800_000_000, "0475/0465"),
            "10000000-0000-0000-0000-000000000002", "process-two"),
    "20000000-0000-0000-0000-000000000002", BUILD, Writer("db-two", "writer-two", True, True, True),
    Ledger(digest("d"), digest("c"), (116, 475), (20, 465), True, 1_800_000_000_000_001), "ready-two",
)
PINS = {pin.runtime.target: pin for pin in (ORIGINAL, RETAINED)}


class Fixture:
    def __init__(self):
        self.calls = []
        self.released = set()
        self.probes = {}
        self.reads = {}

    def private_get(self, target, service, path, *, request, max_body_bytes):
        pin = PINS[target]
        assert service == target.service_name and max_body_bytes > 0
        after = target in self.released
        self.calls.append((target.shape, path, after))
        status, body = 200, b'{"status":"ok"}'
        if path == "/readyz":
            status = 200 if after else 503
            gateway = "ready" if after else ("down" if pin is RETAINED else "starting")
            body = json.dumps({
                "build_revision": pin.build.build_revision, "build_id": pin.build.build_id,
                "deploy_id": pin.build.deploy_id,
                "components": [["process", "ready"], ["database", "ready"], ["gateway", gateway]],
            }).encode()
        reply = Probe(request, pin.runtime, (pin.worker_version,), status, body)
        return self.probes.get((target.shape, path, after), lambda value: value)(reply)

    def readback(self, target, *, request):
        pin = PINS[target]
        after = target in self.released
        self.calls.append((target.shape, "readback", after))
        session = pin.mock_session
        activity = MockActivity(session, (session, 1), session, 2) if after else MockActivity()
        observed = Readback(request, pin.runtime, BUILD.image_digest, pin.writer, pin.ledger, activity)
        return self.reads.get((target.shape, after), lambda value: value)(observed)

    def release(self, target, session):
        self.calls.append((target.shape, "release", session))
        self.released.add(target)
        return True

    def verify(self, original=ORIGINAL, retained=RETAINED):
        return verify_paired_readiness(original, retained, private_get=self.private_get,
                                       readback=self.readback, release_ready=self.release)


def change_body(reply, **changes):
    body = json.loads(reply.body)
    body.update(changes)
    return replace(reply, body=json.dumps(body).encode())


class PairedReadinessTests(unittest.TestCase):
    def test_both_layouts_use_separate_private_bindings_and_ordered_evidence(self):
        fixture = Fixture()
        evidence = fixture.verify()
        calls = []
        for pin, result in zip((ORIGINAL, RETAINED), evidence, strict=True):
            shape = pin.runtime.target.shape
            calls.extend([(shape, "/health", False), (shape, "/readyz", False), (shape, "readback", False),
                          (shape, "release", pin.mock_session), (shape, "/readyz", True), (shape, "readback", True)])
            self.assertEqual(result.reservation, pin)
            self.assertEqual((result.health_status, result.pre_ready_status, result.post_ready_status), (200, 503, 200))
            self.assertEqual(result.activity.checkpoint, (pin.mock_session, 1))
            self.assertEqual(result.activity.command_count, 2)
        self.assertEqual(fixture.calls, calls)

    def test_refuses_unhealthy_or_unpinned_worker_responses(self):
        for path, after, changes, error in (
            ("/health", False, {"status": 503}, "http_status"),
            ("/health", False, {"worker_versions": ()}, "worker_version"),
            ("/readyz", False, {"status": 200}, "http_status"),
            ("/readyz", False, {"worker_versions": (RETAINED.worker_version,)}, "worker_version"),
            ("/readyz", True, {"worker_versions": (ORIGINAL.worker_version,) * 2}, "worker_version"),
            ("/readyz", True, {"body": b'{"build_id":"x","build_id":"y"}'}, "probe_body"),
            ("/readyz", True, {"runtime": replace(ORIGINAL.runtime, process_id="replacement")}, "probe_runtime"),
            ("/readyz", False, {"runtime": RETAINED.runtime}, "probe_runtime"),
            ("/readyz", True, {"request": object()}, "probe_runtime"),
        ):
            with self.subTest(path=path, after=after, changes=changes):
                fixture = Fixture()
                fixture.probes[("0473/0465", path, after)] = lambda reply: replace(reply, **changes)
                with self.assertRaisesRegex(TargetError, error):
                    fixture.verify()
                self.assertEqual(bool(fixture.released), after)

    def test_refuses_stamp_drift_in_either_phase(self):
        for after in (False, True):
            for stamp in ("build_revision", "build_id", "deploy_id"):
                with self.subTest(after=after, stamp=stamp):
                    fixture = Fixture()
                    fixture.probes[("0473/0465", "/readyz", after)] = lambda reply: change_body(reply, **{stamp: "x"})
                    with self.assertRaisesRegex(TargetError, "runtime_stamps"):
                        fixture.verify()

    def test_refuses_malformed_or_incorrect_readiness_components(self):
        for after, names in ((False, ("ready", "ready", "ready")), (False, ("ready", "down", "starting")),
                             (True, ("down", "ready", "ready")), (True, ("ready", "ready", "down"))):
            with self.subTest(after=after, names=names):
                fixture = Fixture()
                pairs = list(zip(("process", "database", "gateway"), names))
                fixture.probes[("0473/0465", "/readyz", after)] = lambda reply: change_body(reply, components=pairs)
                with self.assertRaisesRegex(TargetError, "readiness_components"):
                    fixture.verify()

    def test_refuses_wrong_effective_writer_ledger_digest_or_reservation(self):
        for changes, error in (
            ({"runtime": ORIGINAL.runtime}, "readback_runtime"),
            ({"request": object()}, "readback_runtime"),
            ({"running_digest": digest("f")}, "running_digest"),
            ({"writer": replace(RETAINED.writer, writer_id="other")}, "wrong_pre_writer"),
            ({"writer": replace(RETAINED.writer, isolated=False)}, "unconfirmed_pre_writer"),
            ({"ledger": ORIGINAL.ledger}, "wrong_pre_ledger"),
            ({"ledger": replace(RETAINED.ledger, cutover_fingerprint=digest("f"))}, "wrong_pre_ledger"),
            ({"ledger": replace(RETAINED.ledger, installed_0475_at=None)}, "retained_timestamp"),
            ({"ledger": replace(RETAINED.ledger, installed_0475_at=1_800_000_000_000_000)}, "wrong_pre_ledger"),
        ):
            with self.subTest(changes=changes):
                fixture = Fixture()
                fixture.reads[("0475/0465", True)] = lambda observed: replace(observed, **changes)
                with self.assertRaisesRegex(TargetError, error):
                    fixture.verify()

    def test_refuses_unconfirmed_ready_checkpoint_or_nonempty_rest_sync(self):
        session = ORIGINAL.mock_session
        for after, activity in (
            (False, MockActivity(ready_session=session)),
            (True, MockActivity(session, (session, 1), session, 0)),
            (True, MockActivity(session, (RETAINED.mock_session, 1), session, 2)),
            (True, MockActivity(session, (session, 1), RETAINED.mock_session, 2)),
            (True, MockActivity(session, None, session, 2)),
        ):
            with self.subTest(after=after, activity=activity):
                fixture = Fixture()
                fixture.reads[("0473/0465", after)] = lambda observed: replace(observed, activity=activity)
                with self.assertRaisesRegex(TargetError, "mock_activity"):
                    fixture.verify()

    def test_refuses_shared_scope_or_provider_names_before_probing(self):
        for changes in ({"scope": ORIGINAL.runtime.target.scope}, {"run_id": ORIGINAL.runtime.target.run_id}):
            target = replace(RETAINED.runtime.target, **changes)
            fixture = Fixture()
            with self.assertRaisesRegex(TargetError, "reservations_not_separate"):
                fixture.verify(retained=replace(RETAINED, runtime=replace(RETAINED.runtime, target=target)))
            self.assertEqual(fixture.calls, [])

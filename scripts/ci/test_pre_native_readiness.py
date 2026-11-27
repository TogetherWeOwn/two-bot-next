from dataclasses import replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import ANY, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_readiness import (
        Build, Ledger, MockActivity, Probe, Readback, Reservation, Runtime, Writer, verify_paired_readiness,
    )
    from pre_target import Target, TargetError


DIGEST_PREFIX = "sha256:"
BUILD = Build("a" * 40, "123-1", "pre-deploy", DIGEST_PREFIX + "b" * 64)
ORIGINAL = Reservation(
    Runtime(Target("pre-fixture-original", 1, 1_800_000_000, "0473/0465"),
            "10000000-0000-0000-0000-000000000001", "process-one"),
    "20000000-0000-0000-0000-000000000001", BUILD, Writer("db-one", "writer-one", True, True, True),
    Ledger(DIGEST_PREFIX + "a" * 64, DIGEST_PREFIX + "c" * 64, (115, 473), (20, 465), False, None), "ready-one",
)
RETAINED = Reservation(
    Runtime(Target("pre-fixture-retained", 2, 1_800_000_000, "0475/0465"),
            "10000000-0000-0000-0000-000000000002", "process-two"),
    "20000000-0000-0000-0000-000000000002", BUILD, Writer("db-two", "writer-two", True, True, True),
    Ledger(DIGEST_PREFIX + "d" * 64, DIGEST_PREFIX + "c" * 64, (116, 475), (20, 465), True, 1_800_000_000_000_001), "ready-two",
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
        status, body = 200, b""
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
        calls = []
        for pin, result in zip((ORIGINAL, RETAINED), fixture.verify(), strict=True):
            shape = pin.runtime.target.shape
            calls.extend([(shape, "/health", False), (shape, "/readyz", False), (shape, "readback", False),
                          (shape, "release", pin.mock_session), (shape, "/readyz", True), (shape, "readback", True)])
            self.assertEqual(result.reservation, pin)
            self.assertEqual(result.statuses, (200, 503, 200))
            self.assertEqual(result.activity.checkpoint, (pin.mock_session, 1))
        self.assertEqual(fixture.calls, calls)

    def test_refuses_unhealthy_or_unpinned_worker_responses(self):
        for path, after, changes, error in (
            ("/health", False, {"status": 503}, "http_status"),
            ("/health", False, {"worker_versions": ()}, "worker_version"),
            ("/readyz", False, {"status": 200}, "http_status"),
            ("/readyz", False, {"worker_versions": (RETAINED.worker_version,)}, "worker_version"),
            ("/readyz", True, {"worker_versions": (ORIGINAL.worker_version,) * 2}, "worker_version"),
            ("/readyz", True, {"body": b'{"build_id":"x","build_id":"y"}'}, "probe_body"),
            ("/readyz", True, {"status": 503}, "http_status"),
            ("/readyz", True, {"runtime": replace(ORIGINAL.runtime, process_id="replacement")}, "probe_runtime"),
            ("/readyz", True, {"runtime": replace(ORIGINAL.runtime, process_id=ANY)}, "probe_runtime"),
            ("/readyz", False, {"runtime": RETAINED.runtime}, "probe_runtime"),
            ("/readyz", True, {"request": object()}, "probe_runtime"),
        ):
            with self.subTest(path=path, after=after, changes=changes):
                fixture = Fixture()
                fixture.probes[("0473/0465", path, after)] = lambda reply: replace(reply, **changes)
                with self.assertRaisesRegex(TargetError, error):
                    fixture.verify()

    def test_refuses_malformed_or_incorrect_readiness_components(self):
        for after in (False, True):
            gateway = "ready" if after else "starting"
            pairs = [["process", "ready"], ["database", "ready"], ["gateway", gateway]]
            cases = [({stamp: "x"}, "runtime_stamps") for stamp in ("build_revision", "build_id", "deploy_id")]
            cases += [({"components": value}, "readiness_components") for value in (
                pairs + [pairs[-1]], [pairs[0], ["database", "down"], pairs[2]],
                [["process", "down"], *pairs[1:]], [*pairs[:2], ["gateway", "down" if after else "ready"]],
            )]
            for changes, error in cases:
                with self.subTest(after=after, changes=changes):
                    fixture = Fixture()
                    fixture.probes[("0473/0465", "/readyz", after)] = lambda reply: change_body(reply, **changes)
                    with self.assertRaisesRegex(TargetError, error):
                        fixture.verify()

    def test_refuses_wrong_effective_writer_ledger_digest_or_reservation(self):
        for changes, error in (
            ({"runtime": ORIGINAL.runtime}, "readback_runtime"),
            ({"request": object()}, "readback_runtime"),
            ({"running_digest": DIGEST_PREFIX + "f" * 64}, "running_digest"),
            ({"writer": replace(RETAINED.writer, writer_id="other")}, "wrong_pre_writer"),
            ({"writer": replace(RETAINED.writer, isolated=False)}, "unconfirmed_pre_writer"),
            ({"ledger": replace(RETAINED.ledger, cutover_fingerprint=DIGEST_PREFIX + "f" * 64)}, "wrong_pre_ledger"),
            ({"ledger": replace(RETAINED.ledger, installed_0475_at=None)}, "retained_timestamp"),
            ({"ledger": replace(RETAINED.ledger, installed_0475_at=1_800_000_000_000_000)}, "wrong_pre_ledger"),
        ):
            with self.subTest(changes=changes):
                fixture = Fixture()
                fixture.reads[("0475/0465", True)] = lambda observed: replace(observed, **changes)
                with self.assertRaisesRegex(TargetError, error):
                    fixture.verify()

    def test_refuses_unconfirmed_ready_checkpoint_or_nonempty_rest_sync(self):
        fixture = Fixture()
        with patch.object(fixture, "release", return_value=False), self.assertRaisesRegex(TargetError, "mock_ready_unconfirmed"):
            fixture.verify()
        self.assertNotIn(("0473/0465", "/readyz", True), fixture.calls)
        session = ORIGINAL.mock_session
        for after, activity in (
            (False, MockActivity(ready_session=ANY)),
            (True, MockActivity(session, (session, 1), session, 0)),
            (True, MockActivity(session, (ANY, 1), session, 2)),
            (True, MockActivity(session, (session, 1), RETAINED.mock_session, 2)),
            (True, MockActivity(session, None, session, 2)),
        ):
            with self.subTest(after=after, activity=activity):
                fixture = Fixture()
                fixture.reads[("0473/0465", after)] = lambda observed: replace(observed, activity=activity)
                with self.assertRaisesRegex(TargetError, "mock_activity"):
                    fixture.verify()

    def test_refuses_shared_scope_or_provider_names_before_probing(self):
        first = ORIGINAL.runtime.target
        crossed_targets = (replace(RETAINED.runtime.target, scope=first.scope),
                           replace(RETAINED.runtime.target, run_id=first.run_id))
        crossed = [replace(RETAINED, runtime=replace(RETAINED.runtime, target=target)) for target in crossed_targets]
        crossed.append(replace(RETAINED, writer=replace(RETAINED.writer, writer_id=ORIGINAL.writer.writer_id)))
        for retained in crossed:
            fixture = Fixture()
            with self.assertRaisesRegex(TargetError, "reservations_not_separate"):
                fixture.verify(retained=retained)
            self.assertEqual(fixture.calls, [])

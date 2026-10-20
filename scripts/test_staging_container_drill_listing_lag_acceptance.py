"""Independent, offline acceptance contract for pinned-target listing lag.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts \
    -p 'test_staging_container_drill_listing_lag_acceptance.py' -v

The runtime's build, the application's listed image, and rollout/Worker/namespace
identities are independent inputs. Only transport, ownership and deploy are
faked; Drill.preflight, wait_ready, confirm, leg and image validation stay real.
No implementation patch is included. Intentional RED tests are a handoff, not
permission to merge a red change or dispatch a drill.

Breaks caught: skipping exact-build probes during proven previous-image lag;
trusting unrelated or absent images; accepting regressed or superseded state;
not rechecking identities after a probe; final evidence using an unvalidated
listing or a rollout UUID as the running image; losing the 60s/session gates.
The structured lag flag uses the verifier's existing application_listing_lag
vocabulary, alongside existing rollout_accepted/probes observation and timeline.
"""

import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import test_staging_container_drill as fixtures


drill = fixtures.container_drill
GateError = fixtures.GateError
TARGET_ROLLOUT = "77777777-7777-4777-8777-777777777777"
SUPERSEDING_ROLLOUT = "88888888-8888-4888-8888-888888888888"


class IndependentClient(fixtures.FakeClient):
    """Return independent control-plane and runtime observations over fake time."""

    def __init__(self, world):
        super().__init__(world)
        self.runtime_image = fixtures.PRE_IMAGE
        self.runtime_worker = fixtures.PRE
        self.runtime_revision = None
        self.runtime_build = None
        self.namespace = "ns1"
        self.armed = False
        self.ready_after = 1
        self.readyz = []
        self.after_probe = lambda: None
        self.after_sleep = lambda: None
        self.logs = []
        self.started = world.t

    def api(self, path):
        result = super().api(path)
        if path == fixtures.APPLICATIONS_PATH:
            result[0]["durable_objects"] = {"namespace_id": self.namespace}
        return result

    def request(self, url):
        if url.endswith("/health"):
            return 200, {}, b""
        if not url.endswith("/readyz"):
            raise AssertionError(f"unrecognised offline request: {url}")
        revision, build = self.world.revision_for(self.runtime_image)
        if self.runtime_revision is not None:
            revision = self.runtime_revision
        if self.runtime_build is not None:
            build = self.runtime_build
        ready = not self.armed or len(self.readyz) + 1 >= self.ready_after
        status = 200 if ready else 503
        headers = {"x-two-worker-version": self.runtime_worker}
        body = json.dumps({
            "components": [["process", "ready"],
                           ["gateway", "ready" if ready else "starting"],
                           ["database", "ready"]],
            "build_revision": revision, "build_id": build}).encode()
        if self.armed:
            self.readyz.append((status, self.runtime_worker, revision, build))
            # The response is already captured. Change the control plane now,
            # so only a real post-probe reread can discover the new identity.
            self.after_probe()
        return status, headers, body

    def sleep(self, seconds):
        self.world.tick(seconds)
        if self.world.t - self.started > 310:
            raise AssertionError("offline wait exceeded the existing 300s budget")
        self.after_sleep()

    def install_target(self, *, listing=fixtures.PRE_IMAGE, active_lag=False):
        self.world.tick(1)
        row = fixtures.rollout_row(self.world, TARGET_ROLLOUT,
                                   fixtures.BACKOUT_IMAGE)
        row["current_configuration"] = {"image": fixtures.PRE_IMAGE}
        if active_lag:
            row["health"]["instances"]["active"] = 0
        self.world.rollouts.append(row)
        self.world.serving_image = listing
        self.world.history = [fixtures.BACKOUT_WORKER, fixtures.PRE]
        self.runtime_image = fixtures.BACKOUT_IMAGE
        self.runtime_worker = fixtures.BACKOUT_WORKER
        self.armed = True
        self.readyz.clear()
        self.started = self.world.t

    def supersede(self, image=fixtures.BACKOUT_IMAGE):
        self.world.tick(1)
        row = fixtures.rollout_row(self.world, SUPERSEDING_ROLLOUT, image)
        row["target_version"] = 43
        self.world.rollouts.append(row)


class ListingLagAcceptanceTests(unittest.TestCase):
    def setUp(self):
        # Accidental fallback to live I/O must fail this suite, not reach staging.
        for target in ("socket.create_connection", "subprocess.run", "time.sleep"):
            guard = patch(target, side_effect=AssertionError("live I/O forbidden"))
            guard.start()
            self.addCleanup(guard.stop)

    def scenario(self, *, listing=fixtures.PRE_IMAGE, active_lag=False):
        world = fixtures.World(ready_polls=0)
        client = IndependentClient(world)
        directory = tempfile.TemporaryDirectory(
            prefix="drill-lag-acceptance-",
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        wrangler = root / "wrangler"
        wrangler.mkdir()
        (wrangler / "wrangler.toml").write_text(fixtures.WRANGLER_TOML)

        def deploy(config_path, source_sha):
            config = json.loads(Path(config_path).read_text())
            self.assertEqual(config["env"]["staging"]["containers"][0]["image"],
                             fixtures.BACKOUT_IMAGE)
            self.assertEqual(source_sha, fixtures.BACKOUT_SHA)
            world.deploys.append(fixtures.BACKOUT_IMAGE)
            client.install_target(listing=listing, active_lag=active_lag)
            return 0

        instance = drill.Drill(
            client, fixtures.make_control(world), fixtures.URL, "123456",
            (str(root / "deploy.json"), deploy, str(wrangler)),
            now=lambda: world.t, sleep=client.sleep, log=client.logs.append)
        instance.preflight(fixtures.pin())
        return world, client, instance

    def wait(self, world, instance):
        return instance.wait_ready(
            fixtures.BACKOUT_WORKER, fixtures.BACKOUT_SHA,
            fixtures.BACKOUT_BUILD, fixtures.BACKOUT_IMAGE, world.t)

    def assert_ready(self, world, client, instance):
        try:
            moment, counts, timeline = self.wait(world, instance)
        except GateError as error:
            self.fail(f"expected exact-build readiness, got {error}; "
                      f"actual runtime probes={len(client.readyz)}; "
                      f"{instance._last_wait_observation}")
        self.assertGreater(counts["probes"], 0)
        self.assertGreater(len(client.readyz), 0)
        self.assertTrue(any(status == 200 for status, *_ in client.readyz))
        self.assertTrue(any(entry[1] == 200 and all(entry[2:])
                            for entry in timeline))
        for _, worker, revision, build in client.readyz:
            self.assertEqual((worker, revision, build),
                             (fixtures.BACKOUT_WORKER, fixtures.BACKOUT_SHA,
                              fixtures.BACKOUT_BUILD))
        self.assertLessEqual(moment - client.started, 60)
        return counts

    def assert_rejected(self, world, instance):
        with self.assertRaises(GateError) as caught:
            self.wait(world, instance)
        return str(caught.exception)

    def test_persistent_known_previous_listing_gets_real_exact_build_probes(self):
        for active_lag in (False, True):
            with self.subTest(active_lag=active_lag):
                world, client, instance = self.scenario()
                client.install_target(active_lag=active_lag)
                client.ready_after = 2  # exercise an actual unready runtime first
                counts = self.assert_ready(world, client, instance)
                self.assertEqual(client.readyz[0][0], 503)
                self.assertEqual(world.serving_image, fixtures.PRE_IMAGE)
                self.assertIs(counts.get("application_listing_lag"), True)
                self.assertIn("rollout_accepted=True",
                              instance._last_wait_observation)

    def test_strict_convergence_still_requires_runtime_probes(self):
        world, client, instance = self.scenario()
        client.install_target(listing=fixtures.BACKOUT_IMAGE)
        self.assert_ready(world, client, instance)
        self.assertEqual(world.serving_image, fixtures.BACKOUT_IMAGE)

    def test_unknown_third_image_is_not_treated_as_previous_image_lag(self):
        world, client, instance = self.scenario()
        client.install_target(listing=fixtures.OTHER_IMAGE)
        self.assertEqual(self.assert_rejected(world, instance),
                         "application_image_drift")
        self.assertLessEqual(world.t - client.started, 305)

    def test_listing_regression_after_target_was_seen_is_refused(self):
        world, client, instance = self.scenario()
        client.install_target(listing=fixtures.BACKOUT_IMAGE)
        client.ready_after = 2
        client.after_probe = lambda: setattr(world, "serving_image", fixtures.PRE_IMAGE)
        self.assertEqual(self.assert_rejected(world, instance),
                         "application_image_drift")
        self.assertGreater(len(client.readyz), 0)

    def test_newer_unrelated_rollout_cannot_be_hidden_by_previous_listing(self):
        world, client, instance = self.scenario()
        client.install_target()
        client.supersede(fixtures.OTHER_IMAGE)
        error = self.assert_rejected(world, instance)
        self.assertIn("rollout", error,
                      "supersession must not be misreported as ordinary listing lag")
        self.assertEqual(client.readyz, [])

    def test_same_image_superseding_uuid_after_probe_is_not_the_pinned_rollout(self):
        for listing in (fixtures.PRE_IMAGE, fixtures.BACKOUT_IMAGE):
            with self.subTest(lag=listing == fixtures.PRE_IMAGE):
                world, client, instance = self.scenario()
                client.install_target(listing=listing)

                def supersede_once():
                    client.after_probe = lambda: None
                    client.supersede()

                client.after_probe = supersede_once
                self.assert_rejected(world, instance)
                self.assertGreater(len(client.readyz), 0,
                                   "must exercise the post-probe supersession fence")

    def test_post_probe_worker_deployment_change_is_refused(self):
        for listing in (fixtures.PRE_IMAGE, fixtures.BACKOUT_IMAGE):
            with self.subTest(lag=listing == fixtures.PRE_IMAGE):
                world, client, instance = self.scenario()
                client.install_target(listing=listing)
                client.after_probe = lambda: setattr(world, "history", [fixtures.PRE])
                self.assert_rejected(world, instance)
                self.assertGreater(len(client.readyz), 0,
                                   "must exercise a real probe before Worker drift")

    def test_post_probe_application_namespace_change_is_refused(self):
        for listing in (fixtures.PRE_IMAGE, fixtures.BACKOUT_IMAGE):
            with self.subTest(lag=listing == fixtures.PRE_IMAGE):
                world, client, instance = self.scenario()
                client.install_target(listing=listing)
                client.after_probe = lambda: setattr(client, "namespace", "ns-other")
                self.assert_rejected(world, instance)
                self.assertGreater(len(client.readyz), 0)

    def test_post_probe_worker_binding_change_is_refused(self):
        world, client, instance = self.scenario()
        client.install_target(listing=fixtures.BACKOUT_IMAGE)
        client.after_probe = lambda: world.namespaces.update(
            {fixtures.BACKOUT_WORKER: "ns-other"})
        self.assert_rejected(world, instance)
        self.assertGreater(len(client.readyz), 0)

    def test_missing_control_plane_identities_fail_closed(self):
        for missing in ("listing", "namespace", "worker", "rollout"):
            with self.subTest(missing=missing):
                world, client, instance = self.scenario()
                client.install_target()
                if missing == "listing":
                    world.serving_image = None
                elif missing == "namespace":
                    client.namespace = None
                elif missing == "worker":
                    world.history = [None]
                else:
                    world.rollouts[-1]["id"] = None
                self.assert_rejected(world, instance)
                self.assertEqual(client.readyz, [])

    def test_absent_proof_of_previous_image_is_not_listing_lag(self):
        world, client, instance = self.scenario()
        client.install_target()
        instance.evidence.pop("pre_image")
        world.rollouts[-1]["current_configuration"] = {}
        self.assertEqual(self.assert_rejected(world, instance),
                         "application_image_drift")
        self.assertLessEqual(world.t - client.started, 305)

    def test_runtime_identity_cannot_be_inferred_from_rollout_or_listing(self):
        for missing in ("worker", "revision", "build", "old_build"):
            with self.subTest(missing=missing):
                world, client, instance = self.scenario()
                client.install_target(listing=fixtures.BACKOUT_IMAGE)
                if missing == "worker":
                    client.runtime_worker = None
                elif missing == "revision":
                    client.runtime_revision = ""
                elif missing == "build":
                    client.runtime_build = ""
                else:
                    client.runtime_image = fixtures.PRE_IMAGE
                self.assert_rejected(world, instance)
                self.assertGreater(len(client.readyz), 0)

    def test_previous_image_lag_does_not_make_an_unready_runtime_ready(self):
        world, client, instance = self.scenario()
        client.install_target()
        client.ready_after = 10000
        self.assert_rejected(world, instance)
        self.assertGreater(len(client.readyz), 0,
                           "bounded refusal must still probe the accepted target")
        self.assertLessEqual(world.t - client.started, 305)
        self.assertIn("rollout_accepted=True", instance._last_wait_observation)
        self.assertGreater(len(instance._last_wait_timeline), 0)
        self.assertTrue(all(entry[1] != 200 for entry in instance._last_wait_timeline))

    def test_full_leg_final_image_proof_agrees_with_validated_lag_path(self):
        world, client, instance = self.scenario()
        try:
            evidence = instance.leg("drill", fixtures.pin(),
                                   drill.validate_attestation(fixtures.COVERING_ATTESTATION))
        except GateError as error:
            self.fail(f"expected a witnessed offline leg through listing lag, got {error}; "
                      f"actual runtime probes={len(client.readyz)}")
        self.assertEqual(evidence["running_image"], fixtures.BACKOUT_IMAGE)
        self.assertNotEqual(evidence["running_image"], TARGET_ROLLOUT)
        self.assertEqual(evidence["intended_image"], fixtures.BACKOUT_IMAGE)
        self.assertEqual(world.serving_image, fixtures.PRE_IMAGE)
        self.assertIs(evidence.get("application_listing_lag"), True)
        self.assertGreater(evidence["probes"], 0)
        self.assertGreater(len(client.readyz), 0)
        self.assertEqual(evidence["session_witness"]["status"], "SINGLE_WITNESSED")
        self.assertTrue(evidence["budget_met"])
        self.assertLessEqual(evidence["time_to_ready_seconds"], 60)

    def test_final_leg_cannot_substitute_the_pin_for_a_new_third_image(self):
        world, client, instance = self.scenario(listing=fixtures.BACKOUT_IMAGE)
        real_wait = instance.wait_ready

        def change_after_validated_wait(*args, **kwargs):
            result = real_wait(*args, **kwargs)
            # Instrument the boundary, not its result: every runtime probe,
            # confirmation and rollout check above ran in production code.
            world.serving_image = fixtures.OTHER_IMAGE
            return result

        with patch.object(instance, "wait_ready", side_effect=change_after_validated_wait):
            with self.assertRaises(GateError) as caught:
                instance.leg("drill", fixtures.pin(),
                             drill.validate_attestation(fixtures.COVERING_ATTESTATION))
        self.assertIn(str(caught.exception),
                      ("application_image_drift", "running_image_mismatch"))
        self.assertGreater(len(client.readyz), 0)
        self.assertNotIn("session_witness", instance.evidence["drill"])

    def test_readiness_after_sixty_seconds_is_not_reported_within_budget(self):
        world, client, instance = self.scenario(listing=fixtures.BACKOUT_IMAGE)
        client.ready_after = 15
        evidence = instance.leg(
            "drill", fixtures.pin(),
            drill.validate_attestation(fixtures.COVERING_ATTESTATION))
        self.assertGreater(evidence["time_to_ready_seconds"], 60)
        self.assertFalse(evidence["budget_met"])
        self.assertEqual(instance.evidence["budget_seconds"], 60)
        self.assertEqual(evidence["running_image"], fixtures.BACKOUT_IMAGE)
        self.assertGreater(evidence["probes"], 0)

    def test_probe_through_lag_does_not_invent_a_session_witness(self):
        world, client, instance = self.scenario()
        with self.assertRaises(GateError) as caught:
            instance.leg("drill", fixtures.pin(), None)
        self.assertEqual(str(caught.exception), "session_not_proven")
        self.assertGreater(len(client.readyz), 0)
        self.assertEqual(instance.evidence["drill"]["session_witness"],
                         {"status": "NOT_PROVEN", "reason": "session_log_count_absent"})


if __name__ == "__main__":
    unittest.main()

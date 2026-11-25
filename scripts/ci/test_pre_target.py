"""Synthetic config and reaper proofs with no provider transport."""

from dataclasses import replace
import hashlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_target import (Action, Inventory, Resource, Target, TargetError, confirm_container_stopped,
                            make_config, plan_cleanup, verify_cleanup)


TARGET = Target("pre-fixture-test", 123, 1_800_000_000, "0473/0465")
APP_ID = "5cb771af-3421-4223-8a06-39e061f06400"
WORKER = Resource(TARGET.worker_name, TARGET.worker_name)
CONTAINER = Resource(TARGET.container_name, APP_ID)
ABSENT = Inventory(TARGET, None, None, 0, False, True, startup_settled=True, assigned_count=0)


class FakeTransport:
    def __init__(self, *snapshots):
        self.snapshots = {item.target: item for item in snapshots}
        self.calls = []

    def inventory(self, target):
        return self.snapshots[target]

    def apply(self, target, action):
        snapshot = self.inventory(target)
        if action == Action("revoke_write_access", target.service_name, target.service_name):
            updated = replace(snapshot, write_access=False)
        elif snapshot.worker and action == Action("delete_worker", snapshot.worker.name, snapshot.worker.id):
            assert snapshot.write_access is False
            updated = replace(snapshot, worker=None)
        elif snapshot.container and action == Action("delete_container", snapshot.container.name, snapshot.container.id):
            confirm_container_stopped(target, action, self.inventory(target))
            updated = replace(snapshot, container=None)
        else:
            raise AssertionError("unexpected cleanup action")
        self.calls.append(action.operation)
        self.snapshots[target] = updated


class PreTargetTests(unittest.TestCase):
    def test_config_preserves_artifacts_and_uses_only_disposable_names(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            files = {"worker/index.js": b"export default {};", "worker/part.mjs": b"export {};",
                     "image.oci.tar": b"synthetic OCI archive bytes"}
            (root / "worker").mkdir()
            for name, data in files.items():
                (root / name).write_bytes(data)
            revision = "a" * 40
            digest = "sha256:" + "b" * 64
            receipt = {"source_sha": revision, "worker_message": revision, "BOT_BUILD_REVISION": revision,
                       "image_manifest_digest": digest,
                       "worker_modules_sha256": {name.removeprefix("worker/"): hashlib.sha256(data).hexdigest()
                                                 for name, data in files.items() if name.startswith("worker/")}}
            for target in (TARGET, replace(TARGET, run_id=124, shape="0475/0465")):
                with self.subTest(shape=target.shape):
                    args = dict(now=target.expires_at - 1, receipt=receipt, material_root=root,
                                synthetic_service=target.service_name)
                    config = make_config(target, **args)
                    name = f"two-bot-next-pre-{target.run_id}-1800003600"
                    self.assertEqual(config["name"], name)
                    self.assertEqual(config["containers"], [{"name": name + "-c", "class_name": "TwoBotContainer",
                                     "image": f"registry.pre-fixture-test.invalid/{name}-c@{digest}",
                                     "instance_type": "lite", "max_instances": 1}])
                    self.assertEqual(config["base_dir"], str(root / "worker"))
                    self.assertEqual(config["main"], str(root / "worker/index.js"))
                    self.assertTrue(config["no_bundle"] and config["preserve_file_names"])
                    self.assertEqual(set(config["vars"]), {"BOT_PORT", "CONTAINER_RELEASE", "DATABASE_URL"})
                    self.assertIn(target.shape.replace("/", "-"), config["vars"]["DATABASE_URL"])
                    self.assertFalse(config["workers_dev"] or config["preview_urls"])
                    self.assertFalse(set(config) & {"env", "route", "routes", "triggers", "build"})
                    with self.assertRaises(TypeError):
                        make_config(target, **args, env="staging", name=name)
                    with self.assertRaisesRegex(TargetError, "pre_admission_expired"):
                        make_config(target, **(args | {"now": target.expires_at}))
                    with self.assertRaisesRegex(TargetError, "wrong_pre_service"):
                        make_config(target, **(args | {"synthetic_service": "two-bot-next-staging"}))
                    for field, value in (("source_sha", "c" * 40), ("worker_message", "c" * 40),
                                         ("BOT_BUILD_REVISION", "c" * 40), ("image_manifest_digest", "latest")):
                        wrong = receipt | {field: value}
                        with self.subTest(field=field), self.assertRaisesRegex(TargetError, "wrong_pre_build_identity"):
                            make_config(target, **(args | {"receipt": wrong}))
            self.assertEqual({name: (root / name).read_bytes() for name in files}, files)

    def test_cleanup_spares_active_run_and_removes_late_appearance(self):
        active_target = replace(TARGET, run_id=124, started_at=TARGET.started_at + 60, shape="0475/0465")
        active = Inventory(active_target, Resource(active_target.worker_name, active_target.worker_name),
                           Resource(active_target.container_name, "bcf0c525-7a87-4741-9bd7-cde8a22e4cb0"),
                           1, True, True, assigned_count=1)
        fake = FakeTransport(replace(ABSENT, write_access=True, startup_settled=False,
                                     instance_count=None, assigned_count=None), active)

        def sweep(target, *, status="active"):
            plan = plan_cleanup(target, fake.inventory(target), now=TARGET.expires_at, run_status=status)
            if plan is not None:
                for action in plan:
                    fake.apply(target, action)
                verify_cleanup(target, fake.inventory(target))

        sweep(active_target)
        with self.assertRaisesRegex(TargetError, "pre_startup_unsettled"):
            sweep(TARGET)
        self.assertEqual(fake.calls, ["revoke_write_access"])
        # The lost deploy reply had carried no IDs. Exact scoped inventory now
        # finds the late resources, with their synthetic write access revoked.
        fake.snapshots[TARGET] = replace(fake.inventory(TARGET), worker=WORKER, container=CONTAINER,
                                         instance_count=1, assigned_count=1)
        with self.assertRaisesRegex(TargetError, "pre_container_stop_unconfirmed"):
            sweep(TARGET)
        self.assertEqual(fake.calls, ["revoke_write_access", "delete_worker"])
        # An independent authority stops the instance and confirms zero counts
        # while the exact app is still readable. Deletion preserves that proof.
        fake.snapshots[TARGET] = replace(fake.inventory(TARGET), instance_count=0, assigned_count=0,
                                         startup_settled=True)
        sweep(TARGET)
        sweep(TARGET)
        self.assertEqual(fake.calls, ["revoke_write_access", "delete_worker", "delete_container"])
        self.assertEqual(fake.inventory(TARGET), ABSENT)
        self.assertEqual(fake.inventory(active_target), active)

    def test_cleanup_refuses_untrusted_identity_or_status(self):
        for snapshot in (replace(ABSENT, target=replace(TARGET, scope="pre-fixture-other")),
                         replace(ABSENT, target=replace(TARGET, run_id=124)),
                         replace(ABSENT, target=replace(TARGET, started_at=TARGET.started_at + 1)),
                         replace(ABSENT, target=replace(TARGET, shape="0475/0465")),
                         replace(ABSENT, worker=Resource("two-bot-next-staging", TARGET.worker_name)),
                         replace(ABSENT, worker=Resource(TARGET.worker_name, "two-bot-next-production")),
                         replace(ABSENT, container=Resource("two-bot-next-twobotcontainer-staging", APP_ID)),
                         replace(ABSENT, container=Resource(TARGET.container_name, "../foreign"))):
            with self.subTest(snapshot=snapshot), self.assertRaisesRegex(TargetError, "wrong_pre_"):
                plan_cleanup(TARGET, snapshot, now=TARGET.expires_at, run_status="terminal")
        with self.assertRaisesRegex(TargetError, "unconfirmed_pre_run_status"):
            plan_cleanup(TARGET, ABSENT, now=TARGET.expires_at, run_status="unknown")

    def test_cleanup_readback_refuses_unknown_or_remaining_resources(self):
        self.assertIsNone(verify_cleanup(TARGET, ABSENT))
        for snapshot, reason in (
            (replace(ABSENT, complete=False), "unconfirmed_pre_inventory"),
            (replace(ABSENT, instance_count=None), "pre_cleanup_unconfirmed"),
            (replace(ABSENT, instance_count=False), "unconfirmed_pre_inventory"),
            (replace(ABSENT, assigned_count=None), "pre_cleanup_unconfirmed"),
            (replace(ABSENT, write_access=None), "unconfirmed_pre_inventory"),
            (replace(ABSENT, startup_settled=False), "pre_startup_unsettled"),
            (replace(ABSENT, instance_count=1), "pre_cleanup_unconfirmed"),
            (replace(ABSENT, assigned_count=1), "pre_cleanup_unconfirmed"),
            (replace(ABSENT, container=CONTAINER), "pre_cleanup_unconfirmed"),
            (replace(ABSENT, worker=WORKER), "pre_cleanup_unconfirmed"),
            (replace(ABSENT, write_access=True), "pre_cleanup_unconfirmed"),
        ):
            with self.subTest(snapshot=snapshot), self.assertRaisesRegex(TargetError, reason):
                verify_cleanup(TARGET, snapshot)

    def test_terminal_run_revokes_access_before_deleting_exact_resources(self):
        inventory = replace(ABSENT, worker=WORKER, container=CONTAINER, instance_count=1, write_access=True)
        actions = plan_cleanup(TARGET, inventory, now=TARGET.started_at, run_status="terminal")
        self.assertEqual(actions, (Action("revoke_write_access", TARGET.service_name, TARGET.service_name),
                                  Action("delete_worker", WORKER.name, WORKER.id),
                                  Action("delete_container", CONTAINER.name, CONTAINER.id)))

    def test_app_delete_requires_direct_zero_readback_on_exact_app(self):
        action = Action("delete_container", CONTAINER.name, CONTAINER.id)
        stopped = replace(ABSENT, container=CONTAINER)
        self.assertIsNone(confirm_container_stopped(TARGET, action, stopped))
        for snapshot in (replace(stopped, startup_settled=False), replace(stopped, worker=WORKER),
                         replace(stopped, write_access=True), replace(stopped, instance_count=1),
                         replace(stopped, assigned_count=1)):
            with self.subTest(snapshot=snapshot), self.assertRaisesRegex(TargetError, "pre_container_stop_unconfirmed"):
                confirm_container_stopped(TARGET, action, snapshot)
        for snapshot in (ABSENT, replace(stopped, container=replace(CONTAINER, id="bcf0c525-7a87-4741-9bd7-cde8a22e4cb0"))):
            with self.subTest(snapshot=snapshot), self.assertRaisesRegex(TargetError, "wrong_pre_container_delete"):
                confirm_container_stopped(TARGET, action, snapshot)
        for snapshot, reason in (
            (replace(stopped, target=replace(TARGET, scope="pre-fixture-other")), "wrong_pre_inventory_target"),
            (replace(stopped, complete=False), "unconfirmed_pre_inventory"),
            (replace(stopped, instance_count=False), "unconfirmed_pre_inventory"),
        ):
            with self.subTest(snapshot=snapshot), self.assertRaisesRegex(TargetError, reason):
                confirm_container_stopped(TARGET, action, snapshot)

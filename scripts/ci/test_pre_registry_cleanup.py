"""Exercise isolated predecessor registry disposition with synthetic readback."""

from dataclasses import replace
from hashlib import sha256
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_registry_cleanup import (RegistryDisposition, RegistryReadback, RegistryReservation,
                                      registry_cleanup_disposition)
    from pre_target import Inventory, Resource, Target, TargetError


TARGET = Target("pre-fixture-test", 123, 1_800_000_000, "0473/0465")
REPOSITORY = "registry.example.invalid/staging/two-bot-next-attendance-predecessor-staging"
MANIFEST = b'{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}'
DIGEST = "sha256:" + sha256(MANIFEST).hexdigest()
REFERENCE = f"{REPOSITORY}@{DIGEST}"
RESERVATION = RegistryReservation(TARGET, REFERENCE)
ABSENT = RegistryReadback(TARGET, REFERENCE, False)
CLEANUP = Inventory(TARGET, None, None, 0, False, True, startup_settled=True, assigned_count=0)


class PreRegistryCleanupTests(unittest.TestCase):
    def disposition(self, readback=ABSENT, *, inventory=CLEANUP, run_status="active",
                    now=TARGET.expires_at, publication_settled=True):
        return registry_cleanup_disposition(RESERVATION, inventory, readback, run_status=run_status,
                                            now=now, publication_settled=publication_settled)

    def test_interrupted_or_expired_run_reports_shared_digest_unresolved(self):
        retained_tag = f"{REPOSITORY}:retained"
        other = replace(TARGET, run_id=124, started_at=TARGET.started_at + 60)
        shared_reference = RegistryReservation(other, REFERENCE)
        manifests = {REFERENCE: MANIFEST, retained_tag: MANIFEST}
        readback = replace(ABSENT, present=RESERVATION.reference in manifests)
        self.assertIsNone(self.disposition(readback, now=TARGET.expires_at - 1))
        for status, now in (("terminal", TARGET.started_at + 1), ("active", TARGET.expires_at)):
            with self.subTest(status=status):
                self.assertIs(self.disposition(readback, run_status=status, now=now),
                              RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE)
                self.assertEqual(manifests[shared_reference.reference], MANIFEST)
                self.assertEqual(manifests[retained_tag], MANIFEST)

    def test_confirmed_absence_is_idempotent_and_spares_retained_digest(self):
        retained_reference = f"{REPOSITORY.replace('/staging/', '/retained/')}@{DIGEST}"
        manifests = {retained_reference: MANIFEST}
        for _ in range(2):
            readback = replace(ABSENT, present=RESERVATION.reference in manifests)
            self.assertIs(self.disposition(readback), RegistryDisposition.ALREADY_ABSENT)
        self.assertEqual(manifests, {retained_reference: MANIFEST})

    def test_wrong_scope_refuses_even_an_absence_observation(self):
        for readback in (
            replace(ABSENT, target=replace(TARGET, scope="pre-fixture-other")),
            replace(ABSENT, target=replace(TARGET, run_id=124)),
            replace(ABSENT, target=replace(TARGET, started_at=TARGET.started_at + 1)),
            replace(ABSENT, target=replace(TARGET, shape="0475/0465")),
            replace(ABSENT, reference=REFERENCE.replace("registry.", "other.")),
            replace(ABSENT, reference=REFERENCE.replace("/staging/", "/other/")),
            replace(ABSENT, reference=REFERENCE.replace("-predecessor-staging@", "-production@")),
            replace(ABSENT, reference=f"{REPOSITORY}@sha256:{'0' * 64}"),
            replace(ABSENT, reference=f"{REPOSITORY}:retained"),
        ):
            with self.subTest(readback=readback), self.assertRaisesRegex(TargetError, "^wrong_pre_registry_scope$"):
                self.disposition(readback)

    def test_absence_requires_settled_publication_and_verified_target_cleanup(self):
        for snapshot, error in (
            (replace(CLEANUP, startup_settled=False), "pre_startup_unsettled"),
            (replace(CLEANUP, worker=Resource(TARGET.worker_name, TARGET.worker_name)), "pre_cleanup_unconfirmed"),
            (replace(CLEANUP, target=replace(TARGET, run_id=124)), "wrong_pre_inventory_target"),
        ):
            with self.subTest(error=error), self.assertRaisesRegex(TargetError, f"^{error}$"):
                self.disposition(inventory=snapshot)
        with self.assertRaisesRegex(TargetError, "^pre_registry_publication_unsettled$"):
            self.disposition(publication_settled=False)
        for present in (None, 0):
            with self.subTest(present=present), self.assertRaisesRegex(TargetError, "^unconfirmed_pre_registry_readback$"):
                self.disposition(replace(ABSENT, present=present))
        with self.assertRaisesRegex(TargetError, "^unconfirmed_pre_run_status$"):
            self.disposition(run_status="unknown")

    def test_reservation_requires_the_dedicated_digest_reference(self):
        for reference in (REFERENCE.replace("-predecessor-staging@", "-production@"),
                          f"{REPOSITORY}:retained", f"https://{REFERENCE}",
                          f"{'a' * 256}.{REFERENCE}", REFERENCE.replace("@sha256:", "@")):
            with self.subTest(reference=reference):
                with self.assertRaisesRegex(TargetError, "^invalid_pre_registry_reference$"):
                    RegistryReservation(TARGET, reference)

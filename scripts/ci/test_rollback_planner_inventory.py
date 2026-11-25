"""Exercise the exact PRE catalogs with synthetic archived read-only reports."""

from copy import deepcopy
from dataclasses import asdict, replace
from hashlib import sha256, sha384
import json
from pathlib import Path
import unittest

from test_staging_recovery_status import archive_fixture, plan_fixture, status
from ci.staging_rollback_compatibility import MIGRATION_PATHS, git
from production_recovery_takeover import RecoveryPin
from rollback_planner_inventory import PlanArtifact, verify_inventory
from staging_migrate_approval import digest_json


ROOT = str(Path(__file__).resolve().parents[2])
ORIGINAL = "d8b6c1503fd72a7533ea96979f8c2ddc794e6737"
RETAINED = "058141c29cf135304b060d8fd89aa6e21e44a0c6"
LANDED = "28990125adbc3a636c6a70e9a99d8dbd9f77fbcd"
FENCE_SQL = f"{MIGRATION_PATHS[0]}/0475_attendance_recovery_boundary.sql"
WINDOW = "attendance-qualification-1"
FENCE = "f" * 64


def source_row(sha, path):
    version, description = Path(path).stem.split("_", 1)
    checksum = sha384(git(ROOT, "show", f"{sha}:{path}")).hexdigest()
    return {"version": int(version), "description": description.replace("_", " "),
            "sha384": checksum, "sha384_recomputed": checksum}


def populate_ledgers(plan):
    for source, ledger in (("source_migrations", "ledger"), ("source_store_migrations", "ledger_store")):
        installed = [{key: value for key, value in row.items() if key != "sha384_recomputed"}
                     | {"success": True} for row in plan[source]]
        plan[f"{ledger}_before"] = installed
        plan[f"{ledger}_after"] = deepcopy(installed)
    plan["plan_manifest_sha256"] = status.projection_hash(plan)
    plan["store_plan_manifest_sha256"] = status.store_projection_hash(plan)
    return plan


class RollbackPlannerInventoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cutover, store = ([source_row(ORIGINAL, path) for path in git(
            ROOT, "ls-tree", "-r", "--name-only", ORIGINAL, "--", directory).decode().splitlines()]
                          for directory in MIGRATION_PATHS)
        # The retained catalog adds only the landed 0475 SQL. Hosted CI uses
        # protected history without fetching the local retained planner commit.
        cls.original = populate_ledgers({**plan_fixture(), "source_sha": ORIGINAL, "recovery_evidence_ref": WINDOW,
                                         "source_migrations": cutover, "source_store_migrations": store})
        cls.retained = populate_ledgers({**cls.original, "source_sha": RETAINED,
                                         "source_migrations": [*cutover, source_row(LANDED, FENCE_SQL)]})
        cls.target = digest_json(cls.original["target"])
        cls.inverse_pin = RecoveryPin(
            run_id="90", run_attempt=1, candidate_sha="c" * 40, rollback_sha="b" * 40,
            worker_version="a" * 8 + "-aaaa-aaaa-aaaa-" + "a" * 12,
            image_digest="sha256:" + "d" * 64, application_id="e" * 8 + "-eeee-eeee-eeee-" + "e" * 12,
            namespace_id="a" * 32, database_fingerprint=cls.target, expanded_ledgers_sha256="e" * 64,
            restored_ledgers_sha256=digest_json([cls.retained["ledger_before"], cls.retained["ledger_store_before"]]),
            attendance_fence_sha256=FENCE, fence_epoch=7,
        )
        cls.inverse = {"state": "restored", **asdict(cls.inverse_pin)}

    def verify(self, endpoint, plan, *, artifact_changes=None, archive_override=None, **overrides):
        archive = archive_fixture(json.dumps(plan))
        run, artifact_id = ("11", "101") if plan["source_sha"] == ORIGINAL else ("12", "102")
        artifact = PlanArtifact("a" * 40, run, 1, artifact_id, "sha256:" + sha256(archive).hexdigest())
        pins = {"expected_artifact": artifact, "window_id": WINDOW, "migration_target": "staging",
                "target_fingerprint": self.target}
        if endpoint == "retained-pre":
            pins.update(inverse_pin=self.inverse_pin, inverse=self.inverse)
        pins.update(overrides)
        observed = replace(artifact, **(artifact_changes or {}))
        return verify_inventory(endpoint, archive_override if archive_override is not None else archive, observed, **pins)

    def test_attribute_both_exact_catalogs_without_relaxing_staging(self):
        for endpoint, plan, version, count, run in (
            ("original-pre", self.original, 473, 115, "11"), ("retained-pre", self.retained, 475, 116, "12"),
        ):
            with self.subTest(endpoint=endpoint):
                proof = self.verify(endpoint, plan)
                self.assertEqual((proof["cutover_version"], proof["cutover_count"], proof["run_id"]),
                                 (version, count, run))
                self.assertEqual((proof["store_version"], proof["store_count"]), (465, 20))
                self.assertEqual(proof["cutover_sha256"], plan["plan_manifest_sha256"])
                self.assertEqual(proof["store_sha256"], plan["store_plan_manifest_sha256"])
                with self.assertRaisesRegex(status.GateError, "^plan_ledger_unqualified$"):
                    status.plan_projection(plan)
        self.assertEqual(self.verify("retained-pre", self.retained)["attendance_fence_sha256"], FENCE)
        self.assertNotIn("attendance_fence_sha256", self.verify("original-pre", self.original))
        ordinary = status.plan_projection(plan_fixture())
        self.assertEqual((ordinary["cutover_version"], ordinary["store_version"]), (481, 478))

    def test_refuse_swapped_or_unapproved_catalogs(self):
        for endpoint, plan in (("original-pre", self.retained), ("retained-pre", self.original)):
            with self.subTest(endpoint=endpoint), self.assertRaisesRegex(status.GateError, "^rollback_catalog_source_mismatch$"):
                self.verify(endpoint, plan)
        forged = deepcopy(self.original)
        forged["source_migrations"][0].update(sha384="0" * 96, sha384_recomputed="0" * 96)
        with self.assertRaisesRegex(status.GateError, "^rollback_catalog_mismatch$"):
            self.verify("original-pre", populate_ledgers(forged))

    def test_refuse_unknown_failed_or_pending_ledger_rows(self):
        changes = (
            ("ledger_before", "unknown: ledger unreadable", "invalid_api_schema"),
            ("ledger_store_after", [*self.original["ledger_store_before"], {"version": 478}], "rollback_ledger_unqualified"),
            ("ledger_after", [{**row, "success": False} for row in self.original["ledger_before"]], "rollback_ledger_unqualified"),
            ("pending_before", [475], "rollback_pending_migrations"),
            ("pending_store_before", [478], "rollback_pending_migrations"),
        )
        for field, value, reason in changes:
            with self.subTest(field=field), self.assertRaisesRegex(status.GateError, f"^{reason}$"):
                self.verify("original-pre", {**self.original, field: value})

    def test_refuse_wrong_archive_run_attempt_window_or_target(self):
        with self.assertRaisesRegex(status.GateError, "^plan_archive_digest_mismatch$"):
            self.verify("original-pre", self.original, archive_override=b"different archive")
        for field, value, reason in (
            ("run_id", "19", "rollback_artifact_mismatch"), ("run_attempt", 2, "rollback_artifact_invalid"),
            ("artifact_id", "99", "rollback_artifact_mismatch"), ("workflow_sha", "b" * 40, "rollback_artifact_mismatch"),
            ("archive_digest", "sha256:" + "0" * 64, "rollback_artifact_mismatch"),
        ):
            with self.subTest(field=field), self.assertRaisesRegex(status.GateError, f"^{reason}$"):
                self.verify("original-pre", self.original, artifact_changes={field: value})
        with self.assertRaisesRegex(status.GateError, "^rollback_window_mismatch$"):
            self.verify("original-pre", {**self.original, "recovery_evidence_ref": "old-window"})
        with self.assertRaisesRegex(status.GateError, "^rollback_target_mismatch$"):
            self.verify("original-pre", self.original, target_fingerprint="0" * 64)

    def test_retained_receipt_requires_the_bound_inverse_fence(self):
        for inverse in (None, {key: value for key, value in self.inverse.items() if key != "attendance_fence_sha256"},
                        {**self.inverse, "attendance_fence_sha256": "0" * 64}, {**self.inverse, "run_id": "91"}):
            with self.assertRaisesRegex(status.GateError, "^production_inverse_readback_mismatch$"):
                self.verify("retained-pre", self.retained, inverse=inverse)
        for field in ("database_fingerprint", "restored_ledgers_sha256"):
            pin = replace(self.inverse_pin, **{field: "0" * 64})
            with self.subTest(field=field), self.assertRaisesRegex(status.GateError, "^rollback_inverse_inventory_mismatch$"):
                self.verify("retained-pre", self.retained, inverse_pin=pin, inverse={"state": "restored", **asdict(pin)})

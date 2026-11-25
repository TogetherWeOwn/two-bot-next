"""Compare a protected stopped inventory with a same-source rollback rehearsal."""

from copy import deepcopy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import bot_promotion
import rollback_proof
import staging_recovery_status
from test_staging_recovery_status import (
    APP, ENV, IMAGE, ROLLOUT, SOURCE, VERSION, github_fixture, provider_fixture,
)
from test_rollback_proof import NOW, files, run, stop_files


DIGEST = IMAGE.split("@", 1)[1]
CERTIFICATION_RUN = 30
DRILL_RUN = 123
OTHER_VERSION = "55555555-5555-5555-5555-555555555555"


def authenticated_plan(run_id):
    routes = github_fixture()
    if run_id == "14":
        # A different successful plan dispatch can produce the same clean ledgers.
        routes = {path.replace("actions/runs/13", "actions/runs/14"): value
                  for path, value in routes.items()}
        routes["actions/runs/14"]["id"] = 14
        routes["actions/runs/14/artifacts?per_page=2"]["artifacts"][0]["id"] = 18
        routes["actions/artifacts/18/zip"] = routes.pop("actions/artifacts/17/zip")
    return staging_recovery_status.verified_plan(run_id, lambda path, **_: routes[path])


def inventory_receipt(plan, *, running=False):
    routes = provider_fixture()

    class Provider:
        def api(self, path):
            return deepcopy(routes[path])

    def ownership(_token, fingerprint):
        return {"deploymentId": VERSION, "running": running,
                "owner": {"deploymentId": VERSION, "phase": "active", "epoch": 7},
                "runtimeDatabaseFingerprint": fingerprint}

    return staging_recovery_status.read_status(
        ENV, staging_recovery_status.checked_plan_summary(plan),
        client_factory=lambda *args, **kwargs: Provider(), read=ownership)


def certification(attempt=1):
    build = {"revision": SOURCE, "build_id": f"{CERTIFICATION_RUN}-{attempt}",
             "deploy_id": f"2026.11.23.1+{SOURCE[:7]}"}
    return {"schema_version": 1, "repository": {"id": 1, "full_name": bot_promotion.REPO},
            "source": {"sha": SOURCE, "run_id": CERTIFICATION_RUN, "run_attempt": attempt,
                       "branch": "main", "workflow": ".github/workflows/deploy-staging.yml"},
            "build": build,
            "staging": {**build, "worker_version": VERSION, "application_id": APP, "rollout_id": ROLLOUT,
                        "target_version": 7, "image": IMAGE, "readyz": 200, "health": 200},
            "container": {"image": IMAGE, "digest": DIGEST},
            "worker": {"entrypoint": "index.js", "compatibility_date": "2026-09-29",
                       "compatibility_flags": ["nodejs_compat"],
                       "modules": [{"path": "index.js", "size": 0, "sha256": "e" * 64}]}}


def drill(run_id=DRILL_RUN, *, restored_version=VERSION, restored_image=IMAGE, release_version=None):
    release_version = release_version or restored_version
    child = dict(run(), id=run_id, head_sha=SOURCE)
    rollout, shutdown = files(), stop_files()
    rollout["staging-rollback-snapshot.json"].update(
        serving_version=restored_version, serving_image=restored_image, serving_sha=SOURCE,
        serving_rollout_id=ROLLOUT, application_id=APP)
    rollout["auto-rollback-evidence.json"].update(
        restored_version=restored_version, restored_image_digest=restored_image.split("@", 1)[1])
    rollout["evidence.json"]["revision"] = SOURCE
    acquired_at = int(NOW.timestamp()) * 1000
    expires_at = acquired_at + 120 * 60 * 1000
    rollout["staging-use.jsonl"][0].update(runId=str(run_id), sha=SOURCE, expiresAt=expires_at)
    for row in shutdown["staging-use-release.jsonl"]:
        row["runId"] = str(run_id)
        if "use" in row:
            row.update(deploymentId=release_version, sha=SOURCE, epoch=9)
            row["use"].update(runId=str(run_id), deploymentId=release_version, sha=SOURCE, epoch=9,
                              acquiredAt=acquired_at, expiresAt=expires_at)
    return child, rollout, shutdown


class StagingRecoveryHandoffTests(unittest.TestCase):
    def setUp(self):
        self.plan = authenticated_plan("13")
        self.inventory = inventory_receipt(self.plan)
        self.qualified = certification()
        self.child = drill()

    def validated_restore(self, qualified, child):
        # Validate each artifact against its own successful run or drill attempt
        # before examining the separately pinned handoff below.
        source = qualified["source"]
        bot_promotion._validate_receipt(qualified, source["sha"], source["run_id"], source["run_attempt"], 1)
        run_record, rollout, shutdown = child
        rollback_proof.run_identity(run_record, ".github/workflows/deploy-staging.yml", "workflow_dispatch")
        restored = rollback_proof.validate_evidence(rollout)
        rollback_proof.validate_shutdown(rollout, shutdown, run_record["id"], run_record["head_sha"])
        return restored

    def assert_handoff(self, inventory, qualified, child, restored):
        # These joins express the offline evidence contract. The individual
        # validators accept all the components used in the mismatch cases.
        source = qualified["source"]
        run_record, rollout, shutdown = child
        self.assertEqual((source["run_id"], source["run_attempt"]), (CERTIFICATION_RUN, 1), "qualification attempt")
        self.assertEqual((run_record["id"], run_record["run_attempt"]), (DRILL_RUN, 1), "drill attempt")
        self.assertEqual(inventory["verification"], "full_tuple")
        self.assertEqual(inventory["plan"], self.plan, "protected plan identity")
        self.assertIs(inventory["ownership"]["running"], False, "baseline still running")

        worker, container = inventory["worker"], inventory["container"]
        snapshot = rollout["staging-rollback-snapshot.json"]
        self.assertEqual((worker["source_sha"], inventory["plan"]["source_sha"], source["sha"],
                          snapshot["serving_sha"], run_record["head_sha"]), (SOURCE,) * 5, "same source")
        self.assertEqual((qualified["staging"]["application_id"], qualified["staging"]["rollout_id"]),
                         (container["application_id"], container["rollout_id"]), "qualified rollout")
        self.assertEqual((snapshot["application_id"], snapshot["serving_rollout_id"]),
                         (container["application_id"], container["rollout_id"]), "snapshot rollout")
        pair = (worker["version_id"], container["image_digest"])
        self.assertEqual((qualified["staging"]["worker_version"], qualified["container"]["digest"]), pair)
        self.assertEqual((restored["restored_version"], restored["restored_image_digest"]), pair, "restored pair")
        _, before_release, release = shutdown["staging-use-release.jsonl"]
        self.assertEqual((release["deploymentId"], release["sha"]), (pair[0], SOURCE), "released build")
        use = release["use"]
        self.assertEqual(release["epoch"], before_release["epoch"], "owner epoch before release")
        self.assertEqual((use["deploymentId"], use["sha"], use["epoch"]),
                         (pair[0], SOURCE, release["epoch"]), "released lease binding")

    def test_exact_stopped_pair_restores_and_releases_the_drill_lease(self):
        restored = self.validated_restore(self.qualified, self.child)
        self.assert_handoff(self.inventory, self.qualified, self.child, restored)

    def test_individually_valid_receipts_cannot_be_mixed(self):
        alternate_image = IMAGE.split("@", 1)[0] + "@sha256:" + "f" * 64
        other_lease = drill()
        other_lease[2]["staging-use-release.jsonl"][-1]["use"]["epoch"] = 8
        other_owner = drill()
        release = other_owner[2]["staging-use-release.jsonl"][-1]
        release["epoch"] = release["use"]["epoch"] = 10
        other_candidate = deepcopy(self.inventory)
        other_candidate["worker"]["source_sha"] = "b" * 40
        cases = (
            ("worker version", "restored pair", self.inventory, self.qualified, drill(restored_version=OTHER_VERSION)),
            ("image digest", "restored pair", self.inventory, self.qualified, drill(restored_image=alternate_image)),
            ("plan run", "protected plan identity", inventory_receipt(authenticated_plan("14")),
             self.qualified, self.child),
            ("serving source", "same source", other_candidate, self.qualified, self.child),
            ("certification rerun", "qualification attempt", self.inventory, certification(attempt=2), self.child),
            ("drill run", "drill attempt", self.inventory, self.qualified, drill(run_id=124)),
            ("running inventory", "baseline still running", inventory_receipt(self.plan, running=True),
             self.qualified, self.child),
            ("release for another worker", "released build", self.inventory, self.qualified,
             drill(release_version=OTHER_VERSION)),
            ("release for another epoch", "released lease binding", self.inventory, self.qualified, other_lease),
            ("release and lease from another owner epoch", "owner epoch before release",
             self.inventory, self.qualified, other_owner),
        )
        for case, finding, inventory, qualified, child in cases:
            with self.subTest(case=case):
                restored = self.validated_restore(qualified, child)
                with self.assertRaisesRegex(AssertionError, finding):
                    self.assert_handoff(inventory, qualified, child, restored)

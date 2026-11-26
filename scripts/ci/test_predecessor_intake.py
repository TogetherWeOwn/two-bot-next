"""Exercise the final handoff with two synthetic predecessor builds."""

from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from test_build_predecessor import LABELS, oci_fixture

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from build_predecessor import INPUT_TREE, SOURCE, digest, image_manifest, package_worker
    from pre_target import Target, TargetError, make_config
    from predecessor_intake import admit_predecessor
    from staging_rollout import save


class PredecessorIntakeTests(unittest.TestCase):
    def setUp(self):
        self.scratch = Path(self.enterContext(TemporaryDirectory()))
        self.target = Target("pre-fixture-intake", 789, 1_800_000_000, "0473/0465")
        self.first = self.candidate(123, b"export default { candidate: 1 };")
        self.second = self.candidate(124, b"export default { candidate: 2 };")

    def candidate(self, run_id, worker):
        dry_run = self.scratch / f"dry-{run_id}"
        dry_run.mkdir()
        (dry_run / "index.js").write_bytes(worker)
        root = self.scratch / str(run_id)
        image = root / "image.oci.tar"
        modules = package_worker(dry_run, root / "worker")
        build_id, deploy_id = f"{run_id}-1", f"2026.11.25.1+{SOURCE[:7]}"
        labels = {"org.opencontainers.image.revision": SOURCE, "com.togetherweown.build-id": build_id,
                  "com.togetherweown.deploy-id": deploy_id}
        with patch.dict(LABELS, labels, clear=True):
            oci_fixture(image)
        receipt = {
            "source_sha": SOURCE, "worker_message": SOURCE, "BOT_BUILD_REVISION": SOURCE,
            "BOT_BUILD_ID": build_id, "BOT_DEPLOY_ID": deploy_id, "input_tree": INPUT_TREE,
            "source_archive_sha256": "5" * 64, "worker_modules_sha256": modules,
            "image_manifest_digest": image_manifest(image, labels), "image_archive_sha256": digest(image),
        }
        # The verifier returns the receipt and the materializer copies it into
        # build.json in the private directory alongside the original payloads.
        save(root / "build.json", receipt)
        return {"expected_producer_run_id": run_id, "verified_receipt": receipt, "material_root": root,
                "plan": self.plan(root, receipt)}

    def plan(self, root, receipt):
        return make_config(self.target, now=self.target.started_at, receipt=receipt, material_root=root,
                           synthetic_service=self.target.service_name)

    def admit(self, candidate, **overrides):
        options = {"now": self.target.started_at} | candidate | overrides
        return admit_predecessor(self.target, **options)

    def test_admits_coherent_candidates_but_refuses_crossed_plans(self):
        self.assertNotEqual(self.first["plan"]["base_dir"], self.second["plan"]["base_dir"])
        self.assertNotEqual(self.first["plan"]["containers"], self.second["plan"]["containers"])
        for candidate, other in ((self.first, self.second), (self.second, self.first)):
            root, receipt = candidate["material_root"], candidate["verified_receipt"]
            self.assertEqual(self.admit(candidate), candidate["plan"])
            for plan in (other["plan"], self.plan(root, other["verified_receipt"]),
                         self.plan(other["material_root"], receipt)):
                with self.assertRaisesRegex(TargetError, "^predecessor_plan_mismatch$"):
                    self.admit(candidate, plan=plan)

    def test_refuses_unexpected_producer_and_crossed_manifest(self):
        with self.assertRaisesRegex(TargetError, "^unexpected_predecessor_build$"):
            self.admit(self.first, expected_producer_run_id=self.second["expected_producer_run_id"])
        with self.assertRaisesRegex(TargetError, "^unexpected_predecessor_build$"):
            self.admit(self.first, verified_receipt=self.first["verified_receipt"] | {"source_sha": "a" * 40})
        with self.assertRaisesRegex(TargetError, "^predecessor_handoff_manifest_mismatch$"):
            self.admit(self.first, material_root=self.second["material_root"])

    def test_refuses_foreign_payloads_behind_expected_paths(self):
        root, other_root = self.first["material_root"], self.second["material_root"]
        for name, payload in (("worker/index.js", "worker"), ("image.oci.tar", "image")):
            path = root / name
            original = path.read_bytes()
            with self.subTest(payload=payload):
                path.write_bytes((other_root / name).read_bytes())
                with self.assertRaisesRegex(TargetError, f"^predecessor_handoff_{payload}_mismatch$"):
                    self.admit(self.first)
                path.write_bytes(original)

    def test_refuses_ambiguous_manifest_and_expired_admission(self):
        with self.assertRaisesRegex(TargetError, "^pre_admission_expired$"):
            self.admit(self.first, now=self.target.expires_at)
        (self.first["material_root"] / "build.json").write_bytes(b'{"source_sha": "a", "source_sha": "b"}')
        with self.assertRaisesRegex(TargetError, "^invalid_predecessor_handoff_manifest$"):
            self.admit(self.first)

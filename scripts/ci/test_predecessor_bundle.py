"""Exercise the offline handoff from the protected predecessor build."""

from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import sys
import tempfile
import unittest
from zipfile import ZIP_DEFLATED, ZIP_STORED, ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from predecessor_bundle import (BUILD, IMAGE, INPUT_TREE, REPOSITORY_ID, SOURCE, WORKFLOW, GateError,
                                verify_predecessor_bundle)
from rollback_proof import REPO
from test_build_predecessor import oci_fixture

WORKFLOW_SHA = "d" * 40


def fixture(scratch, run_id, worker_byte, *, transform_layer=None):
    image = scratch / "image.tar"
    build_id, deploy_id = f"{run_id}-1", f"2026.11.25.1+{SOURCE[:7]}"
    labels = {"org.opencontainers.image.revision": SOURCE, "com.togetherweown.build-id": build_id,
              "com.togetherweown.deploy-id": deploy_id}
    manifest = oci_fixture(image, labels=labels, transform_layer=transform_layer)
    worker = {"index.js": worker_byte, "assets/module.wasm": b"wasm"}
    members = {f"worker/{name}": data for name, data in worker.items()}
    members[IMAGE] = image.read_bytes()
    receipt = {
        "source_sha": SOURCE, "worker_message": SOURCE, "input_tree": INPUT_TREE,
        "source_archive_sha256": "5" * 64,
        "worker_modules_sha256": {name: sha256(data).hexdigest() for name, data in worker.items()},
        "image_manifest_digest": manifest, "image_archive_sha256": sha256(members[IMAGE]).hexdigest(),
        "BOT_BUILD_REVISION": SOURCE, "BOT_BUILD_ID": build_id, "BOT_DEPLOY_ID": deploy_id,
    }
    return members, receipt


def pack(members, receipt, *, compression=ZIP_STORED):
    archive = BytesIO()
    with ZipFile(archive, "w", compression=compression) as bundle:
        bundle.writestr(BUILD, json.dumps(receipt))
        for name, data in members.items():
            bundle.writestr(name, data)
    return archive


def metadata(archive, run_id, artifact_id):
    repository = {"id": REPOSITORY_ID, "full_name": REPO}
    run = {"id": run_id, "run_attempt": 1, "path": WORKFLOW, "event": "workflow_dispatch",
           "head_branch": "main", "head_sha": WORKFLOW_SHA, "repository": repository,
           "head_repository": repository, "status": "completed", "conclusion": "success"}
    digest = "sha256:" + sha256(archive.getbuffer()).hexdigest()
    artifact = {"id": artifact_id, "name": f"attendance-predecessor-build-{run_id}-1", "digest": digest,
                "expired": False, "size_in_bytes": len(archive.getbuffer()),
                "workflow_run": {"id": run_id, "head_sha": WORKFLOW_SHA, "head_branch": "main",
                                 "repository_id": REPOSITORY_ID, "head_repository_id": REPOSITORY_ID}}
    return {"run_id": run_id, "workflow_sha": WORKFLOW_SHA, "artifact_id": artifact_id, "archive_digest": digest,
            "run": run, "artifact": artifact}


class PredecessorBundleTests(unittest.TestCase):
    def setUp(self):
        with tempfile.TemporaryDirectory() as scratch:
            self.a = fixture(Path(scratch), 123, b"a")
            self.b = fixture(Path(scratch), 124, b"b")
        self.zip_a = pack(*self.a)
        self.zip_b = pack(*self.b, compression=ZIP_DEFLATED)
        self.pin_a = metadata(self.zip_a, 123, 457)
        self.pin_b = metadata(self.zip_b, 124, 458)

    def test_coherent_bundles_pass(self):
        self.assertEqual(verify_predecessor_bundle(self.zip_a, **self.pin_a), self.a[1])
        self.assertEqual(verify_predecessor_bundle(self.zip_b, **self.pin_b), self.b[1])

    def test_archive_requires_exact_oci_layer_bytes(self):
        normal_receipt = verify_predecessor_bundle(self.zip_b, **self.pin_b)
        for transform, reason in (
            (lambda data: data[:-1], "image_layer_size_mismatch"),
            (lambda data: bytes([data[0] ^ 1]) + data[1:], "image_layer_digest_mismatch"),
            (lambda data: None, "image_layer_missing"),
        ):
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as scratch:
                members, receipt = fixture(Path(scratch), 124, b"b", transform_layer=transform)
                self.assertEqual(receipt["image_manifest_digest"], normal_receipt["image_manifest_digest"])
                archive = pack(members, receipt, compression=ZIP_DEFLATED)
                with self.assertRaisesRegex(GateError, f"^{reason}$"):
                    verify_predecessor_bundle(archive, **metadata(archive, 124, 458))

    def test_wrong_run_attempt_or_artifact_is_refused(self):
        with self.assertRaisesRegex(GateError, "^invalid_predecessor_run$"):
            verify_predecessor_bundle(self.zip_a, **{**self.pin_a, "run": self.pin_b["run"]})
        rerun = {**self.pin_a["run"], "run_attempt": 2}
        with self.assertRaisesRegex(GateError, "^invalid_predecessor_run$"):
            verify_predecessor_bundle(self.zip_a, **{**self.pin_a, "run": rerun})
        with self.assertRaisesRegex(GateError, "^invalid_predecessor_artifact$"):
            verify_predecessor_bundle(self.zip_b, **{**self.pin_a, "artifact": self.pin_b["artifact"]})
        foreign_lineage = {**self.pin_a["artifact"], "workflow_run": self.pin_b["artifact"]["workflow_run"]}
        with self.assertRaisesRegex(GateError, "^invalid_predecessor_artifact$"):
            verify_predecessor_bundle(self.zip_a, **{**self.pin_a, "artifact": foreign_lineage})
        # Even if transport supplies a well-formed artifact receipt, the
        # inner build ID must match the authenticated producer run.
        with self.assertRaisesRegex(GateError, "^predecessor_build_identity_mismatch$"):
            verify_predecessor_bundle(self.zip_b, **metadata(self.zip_b, 123, 457))

    def test_swapped_worker_is_refused_even_with_its_matching_hash_map(self):
        members, receipt = self.a
        changed = {**members, "worker/index.js": self.b[0]["worker/index.js"]}
        swapped = pack(changed, {**receipt, "worker_modules_sha256": self.b[1]["worker_modules_sha256"]})
        with self.assertRaisesRegex(GateError, "^predecessor_archive_digest_mismatch$"):
            verify_predecessor_bundle(swapped, **self.pin_a)
        forged_map = metadata(swapped, 123, 457)["artifact"]
        with self.assertRaisesRegex(GateError, "^invalid_predecessor_artifact$"):
            verify_predecessor_bundle(swapped, **{**self.pin_a, "artifact": forged_map})

        changed_worker = pack(changed, receipt)
        with self.assertRaisesRegex(GateError, "^predecessor_worker_digest_mismatch$"):
            verify_predecessor_bundle(changed_worker, **metadata(changed_worker, 123, 457))
        missing = pack({name: data for name, data in members.items() if not name.endswith(".wasm")}, receipt)
        with self.assertRaisesRegex(GateError, "^predecessor_worker_inventory_mismatch$"):
            verify_predecessor_bundle(missing, **metadata(missing, 123, 457))

    def test_swapped_image_is_refused_even_with_both_image_hashes_replaced(self):
        members, receipt = self.a
        other_members, other_receipt = self.b
        changed = {**members, IMAGE: other_members[IMAGE]}
        for digest_fields, reason in (
            ({}, "predecessor_image_archive_digest_mismatch"),
            ({key: other_receipt[key] for key in ("image_archive_sha256", "image_manifest_digest")},
             "image_build_stamp_mismatch"),
        ):
            with self.subTest(reason=reason):
                archive = pack(changed, {**receipt, **digest_fields})
                with self.assertRaisesRegex(GateError, f"^{reason}$"):
                    verify_predecessor_bundle(archive, **metadata(archive, 123, 457))

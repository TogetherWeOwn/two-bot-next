"""Verify the protected attendance predecessor artifact from a local ZIP."""

from hashlib import file_digest
from struct import error as StructError
import tarfile
from zipfile import BadZipFile, ZipFile
from zlib import error as CompressionError

from bot_promotion import (MAX_MEMBER_BYTES, MAX_MODULES, MAX_RECEIPT_BYTES, MAX_TOTAL_BYTES, PromotionError,
                           regular_zip_members, safe_module_path, validate_artifact_run)
from build_identity import is_build_revision_shape, is_deploy_id_shape
from build_predecessor import INPUT_TREE, SOURCE, image_manifest
from rollback_proof import ProofError, run_identity
from staging_recovery_status import DIGEST, HEX64, REPOSITORY_ID, matches
from staging_rollout import GateError, decode, mapping, require

WORKFLOW = ".github/workflows/build-predecessor.yml"
BUILD = "build.json"
IMAGE = "image.oci.tar"
WORKER = "worker/"
# The OCI archive contains compressed layers from a 112 MiB runtime image budget.
MAX_IMAGE_BYTES = 128 * 1024 * 1024
MAX_ARCHIVE_BYTES = MAX_IMAGE_BYTES + MAX_TOTAL_BYTES + MAX_RECEIPT_BYTES + 1024 * 1024


def verify_predecessor_bundle(archive, *, run_id, workflow_sha, artifact_id, archive_digest, run, artifact):
    """Return the build receipt after validating the exact reviewed artifact.

    The pins come from the reviewed handoff. The run and artifact detail are
    authenticated GitHub responses for those IDs. The reviewed workflow SHA
    must include the producer's protected-main check. Keep the seekable ZIP
    private and stable, and use those same bytes after verification.
    """
    require(type(run_id) is int and 0 < run_id < 10**20
            and type(artifact_id) is int and 0 < artifact_id < 10**20
            and is_build_revision_shape(workflow_sha) and matches(DIGEST, archive_digest),
            "invalid_predecessor_pin")
    run = mapping(run)
    repo_ids = [mapping(run.get(key)).get("id") for key in ("repository", "head_repository")]
    require(run.get("path") in (WORKFLOW, f"{WORKFLOW}@main", f"{WORKFLOW}@refs/heads/main")
            and type(run.get("id")) is int and run["id"] == run_id
            and type(run.get("run_attempt")) is int and run["run_attempt"] == 1
            and run.get("status") == "completed" and run.get("conclusion") == "success"
            and run.get("head_sha") == workflow_sha
            and repo_ids == [REPOSITORY_ID, REPOSITORY_ID]
            and all(type(value) is int for value in repo_ids), "invalid_predecessor_run")
    build_id = f"{run_id}-1"
    artifact = mapping(artifact)
    size = artifact.get("size_in_bytes")
    require(type(artifact.get("id")) is int and artifact["id"] == artifact_id
            and artifact.get("name") == f"attendance-predecessor-build-{build_id}"
            and artifact.get("expired") is False and artifact.get("digest") == archive_digest
            and type(size) is int and 0 < size <= MAX_ARCHIVE_BYTES, "invalid_predecessor_artifact")
    try:
        run_identity(run, WORKFLOW, "workflow_dispatch")
    except ProofError:
        raise GateError("invalid_predecessor_run") from None
    try:
        validate_artifact_run(artifact, run_id=run_id, repo_id=REPOSITORY_ID, head_sha=workflow_sha)
    except PromotionError:
        raise GateError("invalid_predecessor_artifact") from None
    try:
        require(archive.seek(0, 2) == size, "predecessor_archive_size_mismatch")
        archive.seek(0)
        require("sha256:" + file_digest(archive, "sha256").hexdigest() == archive_digest,
                "predecessor_archive_digest_mismatch")
        with ZipFile(archive) as bundle:
            return _verify_build(bundle, build_id)
    except (BadZipFile, CompressionError, EOFError, KeyError, OSError, RuntimeError, StructError,
            ValueError, tarfile.TarError):
        raise GateError("invalid_predecessor_archive") from None


def _verify_build(bundle, build_id):
    members = regular_zip_members(bundle)
    require({BUILD, IMAGE, WORKER + "index.js"} <= members.keys()
            and 3 <= len(members) <= MAX_MODULES + 2
            and all(info.filename == info.orig_filename for info in members.values()),
            "invalid_predecessor_members")
    worker = {name[len(WORKER):]: info for name, info in members.items() if name.startswith(WORKER)}
    require(len(worker) == len(members) - 2
            and all(safe_module_path(path) and info.file_size <= MAX_MEMBER_BYTES for path, info in worker.items())
            and sum(info.file_size for info in worker.values()) <= MAX_TOTAL_BYTES
            and 0 < members[BUILD].file_size <= MAX_RECEIPT_BYTES
            and 0 < members[IMAGE].file_size <= MAX_IMAGE_BYTES, "invalid_predecessor_members")

    receipt = mapping(decode(bundle.read(members[BUILD])))
    deploy_id = receipt.get("BOT_DEPLOY_ID")
    require(receipt.get("source_sha") == receipt.get("worker_message") == receipt.get("BOT_BUILD_REVISION") == SOURCE
            and receipt.get("input_tree") == INPUT_TREE and matches(HEX64, receipt.get("source_archive_sha256"))
            and receipt.get("BOT_BUILD_ID") == build_id and is_deploy_id_shape(deploy_id)
            and deploy_id.endswith(f".1+{SOURCE[:7]}"), "predecessor_build_identity_mismatch")
    hashes = mapping(receipt.get("worker_modules_sha256"))
    require(hashes.keys() == worker.keys(), "predecessor_worker_inventory_mismatch")
    for path, info in worker.items():
        with bundle.open(info) as member:
            require(file_digest(member, "sha256").hexdigest() == hashes[path], "predecessor_worker_digest_mismatch")

    with bundle.open(members[IMAGE]) as image:
        require(file_digest(image, "sha256").hexdigest() == receipt.get("image_archive_sha256"),
                "predecessor_image_archive_digest_mismatch")
        image.seek(0)
        labels = {"org.opencontainers.image.revision": SOURCE, "com.togetherweown.build-id": build_id,
                  "com.togetherweown.deploy-id": deploy_id}
        manifest = image_manifest(None, labels, fileobj=image)
    require(manifest == receipt.get("image_manifest_digest"), "predecessor_image_manifest_mismatch")
    return receipt

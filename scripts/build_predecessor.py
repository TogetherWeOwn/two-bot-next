"""Build the reviewed attendance predecessor without accessing a runtime target."""

from datetime import datetime, timezone
from hashlib import file_digest, sha256
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile

from rollback_proof import ProofError
from staging_qualification import check_evidence
from staging_recovery_status import github
from staging_rollout import GateError, decode, mapping, require, save

SOURCE = "b689bc3545777c801b8758bcb7160e72a0321724"
INPUT_TREE = "217e63471c39fc6177e13cefa87693c2a3c1572a"
WORKFLOW = "TogetherWeOwn/two-bot-next/.github/workflows/build-predecessor.yml@refs/heads/main"
WRANGLER = "4.152.0"
DESCRIPTOR_LIMIT = 1024 * 1024
REPOSITORY_ID = "1396224242"


def git(*args):
    return subprocess.check_output(["git", *args], timeout=30, text=True).strip()


def digest(path):
    with path.open("rb") as stream:
        return file_digest(stream, "sha256").hexdigest()


def prepare(root, workflow_sha):
    require(git("rev-parse", "HEAD") == workflow_sha, "workflow_checkout_mismatch")
    git("merge-base", "--is-ancestor", SOURCE, "origin/main")
    check_evidence(SOURCE, github)
    root.mkdir()
    package, output = root / "package", root / "export"
    git("worktree", "add", "--detach", str(package), SOURCE)
    try:
        subprocess.run(["python3", "scripts/predecessor_build_source.py", "--source", SOURCE, str(output)],
                       cwd=package, timeout=60, check=True)
    finally:
        git("worktree", "remove", str(package))
    tree = git("rev-parse", f"{SOURCE}^{{tree}}")
    receipt = mapping(decode((output / "provenance.json").read_bytes()))
    require(receipt.get("archive_sha256") == digest(output / "source.tar")
            and receipt.get("input_tree") == INPUT_TREE and receipt.get("source_tree") == tree
            and receipt.get("source_revision") == receipt.get("worker_message") == SOURCE
            and receipt.get("container_build_args") == {"BOT_BUILD_REVISION": SOURCE}, "export_identity_mismatch")
    with tarfile.open(output / "source.tar", "r:") as tar:
        tar.extractall(root / "source", filter="data")


def archive_json(archive, name, descriptor=None):
    member = archive.getmember(name)
    require(member.isfile() and 0 < member.size <= DESCRIPTOR_LIMIT, "image_metadata_unproved")
    with archive.extractfile(member) as stream:
        raw = stream.read(DESCRIPTOR_LIMIT + 1)
    if descriptor is not None:
        require(member.size == descriptor.get("size")
                and descriptor["digest"] == "sha256:" + sha256(raw).hexdigest(), "image_metadata_digest_mismatch")
    return mapping(decode(raw))


def read_descriptor(archive, descriptor, media_type):
    descriptor = mapping(descriptor)
    address = descriptor.get("digest")
    require(descriptor.get("mediaType") == media_type and isinstance(address, str)
            and re.fullmatch(r"sha256:[0-9a-f]{64}", address), "image_descriptor_unproved")
    return archive_json(archive, f"blobs/sha256/{address[7:]}", descriptor)


def image_manifest(path, labels):
    with tarfile.open(path, "r:") as archive:
        manifests = archive_json(archive, "index.json").get("manifests")
        require(isinstance(manifests, list) and len(manifests) == 1, "image_manifest_ambiguous")
        manifest = read_descriptor(archive, manifests[0], "application/vnd.oci.image.manifest.v1+json")
        config = read_descriptor(archive, manifest.get("config"), "application/vnd.oci.image.config.v1+json")
    actual_labels = mapping(mapping(config.get("config")).get("Labels"))
    require(config.get("os") == "linux" and config.get("architecture") == "amd64"
            and all(actual_labels.get(key) == value for key, value in labels.items()), "image_build_stamp_mismatch")
    return manifests[0]["digest"]


def build(root, run_id):
    source, artifact = root / "source", root / "artifact"
    bundle = artifact / "worker"
    image = artifact / "image.oci.tar"
    artifact.mkdir()
    build_id = f"{run_id}-1"
    deploy_id = datetime.now(timezone.utc).strftime("%Y.%m.%d") + f".1+{SOURCE[:7]}"
    stamps = {"BOT_BUILD_REVISION": SOURCE, "BOT_BUILD_ID": build_id, "BOT_DEPLOY_ID": deploy_id}
    args = [flag for key, value in stamps.items() for flag in ("--build-arg", f"{key}={value}")]
    subprocess.run(["docker", "buildx", "build", "--platform", "linux/amd64", "--provenance=false",
                    "--sbom=false", "--output", f"type=oci,dest={image}", *args, str(source)],
                   timeout=2400, check=True)
    labels = {"org.opencontainers.image.revision": SOURCE, "com.togetherweown.build-id": build_id,
              "com.togetherweown.deploy-id": deploy_id}
    manifest = image_manifest(image, labels)

    worker = source / "wrangler"
    package = mapping(decode((worker / "node_modules/wrangler/package.json").read_bytes()))
    require(package.get("version") == WRANGLER, "archived_wrangler_version_mismatch")
    subprocess.run(["node_modules/.bin/wrangler", "deploy", "--dry-run",
                    "--containers-rollout=none", "--outdir", str(bundle)], cwd=worker,
                   env={**os.environ, "WRANGLER_SEND_METRICS": "false"}, timeout=120, check=True)
    modules = {str(path.relative_to(bundle)): digest(path) for path in sorted(bundle.rglob("*")) if path.is_file()}
    require("index.js" in modules, "worker_bundle_entrypoint_missing")
    save(artifact / "build.json", {"source_sha": SOURCE, "input_tree": INPUT_TREE, "worker_message": SOURCE,
                                  "source_archive_sha256": digest(root / "export/source.tar"),
                                  "worker_modules_sha256": modules, "image_manifest_digest": manifest,
                                  "image_archive_sha256": digest(image), **stamps})


def main(env):
    try:
        run_id = env.get("GITHUB_RUN_ID", "")
        require(env.get("GITHUB_REPOSITORY_ID") == REPOSITORY_ID and env.get("GITHUB_WORKFLOW_REF") == WORKFLOW
                and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch" and env.get("GITHUB_REF_PROTECTED") == "true"
                and env.get("GITHUB_RUN_ATTEMPT") == "1" and re.fullmatch(r"[1-9][0-9]{0,19}", run_id),
                "untrusted_predecessor_dispatch")
        root = Path(env["RUNNER_TEMP"]) / "predecessor"
        if sys.argv[1:] == ["prepare"]:
            prepare(root, env["GITHUB_SHA"])
        else:
            require(sys.argv[1:] == ["build"], "unknown_predecessor_build_operation")
            build(root, run_id)
    except (GateError, ProofError, OSError, ValueError, KeyError, subprocess.SubprocessError, tarfile.TarError) as error:
        message = str(error) if isinstance(error, (GateError, ProofError)) else "invalid or unavailable build evidence"
        print(f"Predecessor build refused: {message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(os.environ))

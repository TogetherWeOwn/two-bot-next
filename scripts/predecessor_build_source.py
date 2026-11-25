#!/usr/bin/env python3
"""Export the reviewed attendance predecessor inputs from an exact checkout.

The protected caller checks main ancestry before using this exporter. Verify
source.tar against provenance.json before unpacking it as the Docker context
and using its wrangler/ directory. Use worker_message as the Wrangler message
and container_build_args for the revision. The caller supplies the run-specific
build and deploy IDs and records the actual Worker version and image digest.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BASE = "d314f4c63ecbea263aa953bc8a649a444a97da7c"
# Tree of the reviewed 84453ad83126e5f27ba55694620ba73b773916d5 candidate.
REVIEWED_TREE = "217e63471c39fc6177e13cefa87693c2a3c1572a"
DELTA = "scripts/fixtures/predecessor-worker.patch"
GIT_TIMEOUT_SECONDS = 30


def git(*args, index=None, data=None, output=subprocess.PIPE):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_NO_REPLACE_OBJECTS="1", GIT_NO_LAZY_FETCH="1")
    if index is not None:
        env["GIT_INDEX_FILE"] = str(index)
    return subprocess.run(["git", *args], cwd=ROOT, env=env, input=data,
                          stdout=output, stderr=subprocess.PIPE, check=True,
                          timeout=GIT_TIMEOUT_SECONDS).stdout


def git_id(ref):
    return git("rev-parse", "--verify", ref).decode("ascii").strip()


def verify_archive(archive, tree):
    # Git archive can honor local export attributes. Verify the actual bytes
    # and executable modes against the tree before issuing a receipt.
    expected = {}
    for record in git("ls-tree", "-rz", tree).split(b"\0")[:-1]:
        metadata, path = record.split(b"\t", 1)
        mode, kind, oid = metadata.split()
        if kind != b"blob" or mode not in (b"100644", b"100755"):
            raise ValueError("reviewed tree must contain regular files only")
        expected[os.fsdecode(path)] = (int(mode, 8) & 0o777, oid)

    with tarfile.open(archive, "r:") as source:
        for entry in source:
            if entry.isdir():
                continue
            if not entry.isreg() or entry.name not in expected:
                raise ValueError("archive has an unexpected entry")
            mode, oid = expected.pop(entry.name)
            # Compare Git's blob ID format. The receipt uses SHA-256 for the tar.
            blob = hashlib.sha1(f"blob {entry.size}\0".encode(), usedforsecurity=False)
            with source.extractfile(entry) as stream:
                actual = hashlib.file_digest(stream, lambda: blob).hexdigest().encode("ascii")
            if actual != oid or entry.mode != mode:
                raise ValueError("archive bytes or file mode differ from the reviewed tree")
    if expected:
        raise ValueError("archive is missing files from the reviewed tree")


def export_source(revision, destination):
    if not re.fullmatch(r"[0-9a-f]{40}", revision) or git_id("HEAD") != revision:
        raise ValueError("source SHA must be the exact checked-out commit")
    if destination.is_relative_to(ROOT) or destination.exists():
        raise ValueError("destination must be a new directory outside the checkout")
    git("merge-base", "--is-ancestor", BASE, revision)
    delta_blob = git_id(f"{revision}:{DELTA}")
    delta = git("cat-file", "blob", delta_blob)

    with tempfile.TemporaryDirectory(dir=destination.parent) as scratch:
        scratch = Path(scratch)
        index = scratch / "index"
        git("read-tree", BASE, index=index)
        git("apply", "--cached", "--unidiff-zero", "-", data=delta, index=index)
        tree = git("write-tree", index=index).decode("ascii").strip()
        if tree != REVIEWED_TREE:
            raise ValueError("reconstructed tree differs from the reviewed predecessor")

        archive = scratch / "source.tar"
        with archive.open("wb") as stream:
            git("-c", "tar.umask=0022", "archive", "--format=tar", tree, output=stream)
        verify_archive(archive, tree)
        with archive.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        receipt = {
            "source_revision": revision,
            "source_tree": git_id(f"{revision}^{{tree}}"),
            "historical_base_revision": BASE,
            "worker_delta_blob": delta_blob,
            "input_tree": tree,
            "archive_sha256": digest,
            "worker_message": revision,
            "container_build_args": {"BOT_BUILD_REVISION": revision},
        }
        destination.mkdir()
        archive.rename(destination / archive.name)
        (destination / "provenance.json").write_text(json.dumps(receipt, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, help="full SHA of the selected checkout")
    parser.add_argument("destination", type=Path, help="new output directory outside the checkout")
    args = parser.parse_args()
    try:
        export_source(args.source, args.destination.resolve())
    except (OSError, ValueError, subprocess.SubprocessError, tarfile.TarError) as error:
        parser.exit(1, f"predecessor build source refused: {error}\n")


if __name__ == "__main__":
    main()

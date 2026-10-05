#!/usr/bin/env python3
"""Reconstruct Docker's dependency layer and validate it without a Docker daemon."""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]

# Pinned dependency-layer inputs: every COPY source before `COPY . .` in the
# Dockerfile. The validator fails closed when the layer gains or loses an
# input, so Dockerfile drift this script does not understand can never pass
# silently. Update this set alongside the Dockerfile; the fixtures in
# `scripts/test_check_docker_manifests.py` pin the same tree.
EXPECTED_SOURCES = frozenset({
    "Cargo.toml",
    "Cargo.lock",
    "crates/core/Cargo.toml",
    "crates/discord/Cargo.toml",
    "crates/bot/Cargo.toml",
    "crates/cutover/Cargo.toml",
    "crates/store/Cargo.toml",
    "crates/testsupport/Cargo.toml",
})


def dependency_sources(docker_text):
    """COPY sources of the dependency layer (everything before `COPY . .`)."""
    sources = []
    for line in docker_text.replace("\\\n", " ").splitlines():
        if line.startswith("COPY . ."):
            break
        if line.startswith("COPY "):
            *srcs, _destination = shlex.split(line)[1:]
            sources.extend(srcs)
    return sources


def check_sources(sources):
    """Fail closed when the layer drifts from the pinned input set."""
    missing = EXPECTED_SOURCES - set(sources)
    extra = set(sources) - EXPECTED_SOURCES
    assert not missing, f"dependency layer lost pinned inputs: {sorted(missing)}"
    assert not extra, f"dependency layer gained unpinned inputs: {sorted(extra)}"


def reconstruct(docker_text, root, dest):
    """Replay the dependency-layer instructions into dest.

    Returns True when the layer ends in the locked fetch. Raises on any
    instruction this validator does not understand, so new Dockerfile verbs
    fail closed instead of passing unchecked.
    """
    fetch_seen = False
    for line in docker_text.replace("\\\n", " ").splitlines():
        if line.startswith("COPY . ."):
            break
        if line.startswith("COPY "):
            *sources, destination = shlex.split(line)[1:]
            target = dest / destination
            target.mkdir(parents=True, exist_ok=True)
            for source in sources:
                shutil.copyfile(root / source, target / Path(source).name)
        elif line.startswith("RUN "):
            for command in line[4:].split("&&"):
                words = shlex.split(command)
                if words[:2] == ["mkdir", "-p"]:
                    for path in words[2:]:
                        (dest / path).mkdir(parents=True, exist_ok=True)
                elif words[0] == "echo" and words[2] == ">":
                    (dest / words[3]).write_text(words[1] + "\n")
                elif words == ["cargo", "fetch", "--locked"]:
                    fetch_seen = True
                else:
                    raise AssertionError(
                        f"unsupported dependency-layer command: {words[0]}")
    return fetch_seen


def validate_layer(dest):
    """Every workspace member must resolve from the replayed layer."""
    manifest = tomllib.loads((dest / "Cargo.toml").read_text())
    assert (dest / "Cargo.lock").is_file(), \
        "Cargo.lock missing from dependency layer"
    members = [".", *manifest["workspace"]["members"]]
    for member in members:
        package = dest / member
        assert (package / "Cargo.toml").is_file(), \
            f"Missing dependency-layer manifest: {member}"
        assert (package / "src/lib.rs").is_file() \
            or (package / "src/main.rs").is_file(), \
            f"Missing dependency-layer target: {member}"
    return len(members)


def run_locked_fetch(dest):
    """The real locked fetch; raises when the layer does not resolve."""
    subprocess.run(
        ["cargo", "metadata", "--offline", "--locked", "--no-deps",
         "--format-version", "1", "--manifest-path", str(dest / "Cargo.toml")],
        check=True, stdout=subprocess.DEVNULL,
    )


def check_tree(docker_text, root, scratch_dir=None, fetch=run_locked_fetch):
    """Full offline validation of one Dockerfile text against one root."""
    sources = dependency_sources(docker_text)
    assert sources, "no dependency-layer COPY sources found"
    check_sources(sources)
    with tempfile.TemporaryDirectory(
            prefix="docker-manifests-", dir=scratch_dir) as directory:
        layer = Path(directory)
        assert reconstruct(docker_text, root, layer), \
            "dependency fetch layer was not checked"
        validate_layer(layer)
        fetch(layer)


def main():
    docker = (ROOT / "Dockerfile").read_text()
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") \
        or os.environ["RUNNER_TEMP"]
    check_tree(docker, ROOT, scratch_dir=scratch)
    print("Docker dependency-layer manifests: PASS (offline, locked)")


if __name__ == "__main__":
    main()

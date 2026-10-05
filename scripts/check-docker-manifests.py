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

# Pinned dependency-layer inputs, derived from the workspace members so the
# pin can never drift from `Cargo.toml`: every COPY source before `COPY . .`
# must be exactly this set. The validator fails closed when the layer gains
# or loses an input, or carries an instruction it does not understand, so
# Dockerfile drift this script does not understand can never pass silently.
def _expected_sources(root):
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    return frozenset({
        "Cargo.toml",
        "Cargo.lock",
        *(f"{member}/Cargo.toml" for member in manifest["workspace"]["members"]),
    })


EXPECTED_SOURCES = _expected_sources(ROOT)


def _dependency_layer_lines(docker_text):
    """Raw dependency-layer lines: after the builder WORKDIR up to `COPY . .`.

    Scoping the scan this way keeps the pinned preamble (`FROM`, the builder
    `WORKDIR`) out of the replay while every instruction inside the layer must
    be an uppercase `COPY`/`RUN`. Anything else (`ADD`, `WORKDIR`, `ENV`,
    `ARG`, lowercase variants) is left in the returned lines for the callers
    to reject, so layer drift fails closed instead of passing unchecked.
    """
    lines = docker_text.replace("\\\n", " ").splitlines()
    start = 0
    for index, line in enumerate(lines):
        if line.strip().upper().startswith("WORKDIR "):
            start = index + 1
            break
    end = len(lines)
    for index in range(start, len(lines)):
        if lines[index].strip().startswith("COPY . ."):
            end = index
            break
    return lines[start:end]


def _layer_instruction(line):
    """Split one layer line into (verb, stripped); blank/comment lines give (None, ...)."""
    stripped = line.strip()
    if not stripped or stripped.startswith("#"):
        return None, stripped
    return stripped.split(None, 1)[0].upper(), stripped


def dependency_sources(docker_text):
    """COPY sources of the dependency layer (everything before `COPY . .`).

    Raises on any layer instruction that is not uppercase `COPY`/`RUN`:
    Dockerfile verbs are case-insensitive, so a lowercase `copy` or an `ADD`
    would otherwise add inputs the pin never sees.
    """
    sources = []
    for line in _dependency_layer_lines(docker_text):
        verb, stripped = _layer_instruction(line)
        if verb is None:
            continue
        if verb == "COPY":
            assert stripped.startswith("COPY "), \
                f"dependency layer must use uppercase COPY: {stripped}"
            *srcs, _destination = shlex.split(stripped)[1:]
            sources.extend(srcs)
        elif verb == "RUN":
            assert stripped.startswith("RUN "), \
                f"dependency layer must use uppercase RUN: {stripped}"
        else:
            raise AssertionError(
                f"unsupported dependency-layer instruction: {stripped}")
    return sources


def check_sources(sources, root=ROOT):
    """Fail closed when the layer drifts from the workspace-derived input set."""
    expected = _expected_sources(root)
    missing = expected - set(sources)
    extra = set(sources) - expected
    assert not missing, f"dependency layer lost pinned inputs: {sorted(missing)}"
    assert not extra, f"dependency layer gained unpinned inputs: {sorted(extra)}"


def reconstruct(docker_text, root, dest):
    """Replay the dependency-layer instructions into dest.

    Returns True when the layer ends in the locked fetch. Raises on any
    instruction this validator does not understand, so new Dockerfile verbs
    fail closed instead of passing unchecked.
    """
    fetch_seen = False
    for line in _dependency_layer_lines(docker_text):
        verb, stripped = _layer_instruction(line)
        if verb is None:
            continue
        if verb == "COPY":
            assert stripped.startswith("COPY "), \
                f"dependency layer must use uppercase COPY: {stripped}"
            *sources, destination = shlex.split(stripped)[1:]
            target = dest / destination
            target.mkdir(parents=True, exist_ok=True)
            for source in sources:
                shutil.copyfile(root / source, target / Path(source).name)
        elif verb == "RUN":
            assert stripped.startswith("RUN "), \
                f"dependency layer must use uppercase RUN: {stripped}"
            for command in stripped[4:].split("&&"):
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
        else:
            raise AssertionError(
                f"unsupported dependency-layer instruction: {stripped}")
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

#!/usr/bin/env python3
"""Reconstruct Docker's dependency layer and validate it without a Docker daemon."""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]

class Refusal(Exception):
    """Fixed-vocabulary refusal for Docker dependency-layer validation.

    Every validation path raises this (never ``assert``/``AssertionError``)
    so the gate fails closed under ``python -O``, where asserts are stripped.
    The message keeps its diagnostic vocabulary so any refusal maps to its
    next action: fix the named Dockerfile line, restore the named pinned
    input, or set the named scratch env var.
    """


def require(condition, message):
    """Raise :class:`Refusal` when condition is false, else return None.

    A function call survives ``python -O``; a bare ``assert`` does not.
    """
    if not condition:
        raise Refusal(message)
    return None


# Bounded-subprocess policy (docs/bounded-subprocess-checks.md): every child
# wait is finite so a stalled child fails closed here instead of hanging
# until the external CI ceiling. Local Git/metadata probes answer in well
# under a second; 30s bounds the stall without touching valid runs.
SUBPROCESS_TIMEOUT_SECONDS = 30


class MetadataFetchError(ValueError):
    """The locked cargo-metadata probe failed, timed out or never spawned."""

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


def _layer_instructions(docker_text):
    """Validated (kind, text) pairs for every instruction before `COPY . .`.

    kind is "preamble" (the pinned builder `FROM` / `WORKDIR /app`), "copy",
    or "run". Blank lines and comments are skipped. The preamble is validated
    too: `FROM` once, then exactly `WORKDIR /app`, then the layer — so a
    `COPY`, `RUN` or `ADD` slipped between `FROM` and `WORKDIR`, a second
    `WORKDIR`, or any other verb (`ENV`, `ARG`, lowercase variants) raises
    instead of passing unchecked. Dockerfile verbs are case-insensitive, so
    only the uppercase spellings this validator understands are accepted.
    """
    from_seen = False
    workdir_seen = False
    layer_started = False
    instructions = []
    for line in docker_text.replace("\\\n", " ").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped.startswith("COPY . ."):
            break
        upper = stripped.split()[0].upper()
        if upper == "COPY":
            require(stripped.startswith("COPY "),
                    f"dependency layer must use uppercase COPY: {stripped}")
            require(from_seen and workdir_seen,
                    f"dependency-layer COPY before the builder preamble: {stripped}")
            layer_started = True
            instructions.append(("copy", stripped))
        elif upper == "RUN":
            require(stripped.startswith("RUN "),
                    f"dependency layer must use uppercase RUN: {stripped}")
            require(from_seen and workdir_seen,
                    f"dependency-layer RUN before the builder preamble: {stripped}")
            layer_started = True
            instructions.append(("run", stripped))
        elif upper == "FROM":
            require(stripped.startswith("FROM "),
                    f"dependency layer must use uppercase FROM: {stripped}")
            require(not from_seen and not layer_started,
                    f"unexpected dependency-layer preamble: {stripped}")
            from_seen = True
            instructions.append(("preamble", stripped))
        elif upper == "WORKDIR":
            require(stripped == "WORKDIR /app",
                    "dependency-layer WORKDIR must be the pinned builder "
                    f"directory: {stripped}")
            require(from_seen and not workdir_seen and not layer_started,
                    f"unexpected dependency-layer preamble: {stripped}")
            workdir_seen = True
            instructions.append(("preamble", stripped))
        else:
            raise Refusal(
                f"unsupported dependency-layer instruction: {stripped}")
    return instructions


def _command_words(command):
    """Keep COPY and RUN quoting failures on the same typed refusal path."""
    try:
        return shlex.split(command)
    except ValueError as error:
        raise Refusal(
            "unsupported dependency-layer command: malformed quoting") from error


def dependency_sources(docker_text):
    """COPY sources of the dependency layer (everything before `COPY . .`).

    Raises on any layer or preamble instruction this validator does not
    understand, so drift anywhere before `COPY . .` fails closed.
    """
    sources = []
    for kind, stripped in _layer_instructions(docker_text):
        if kind != "copy":
            continue
        *srcs, _destination = _command_words(stripped)[1:]
        sources.extend(srcs)
    return sources


def check_sources(sources, root=ROOT):
    """Fail closed when the layer drifts from the workspace-derived input set."""
    expected = _expected_sources(root)
    missing = expected - set(sources)
    extra = set(sources) - expected
    require(not missing,
            f"dependency layer lost pinned inputs: {sorted(missing)}")
    require(not extra,
            f"dependency layer gained unpinned inputs: {sorted(extra)}")


def reconstruct(docker_text, root, dest):
    """Replay the dependency-layer instructions into dest.

    Returns True when the layer ends in the locked fetch. Raises on any
    instruction this validator does not understand, so new Dockerfile verbs
    fail closed instead of passing unchecked.
    """
    fetch_seen = False
    for kind, stripped in _layer_instructions(docker_text):
        if kind == "preamble":
            continue
        if kind == "copy":
            *sources, destination = _command_words(stripped)[1:]
            target = dest / destination
            target.mkdir(parents=True, exist_ok=True)
            for source in sources:
                shutil.copyfile(root / source, target / Path(source).name)
        elif kind == "run":
            for command in stripped[4:].split("&&"):
                words = _command_words(command)
                require(words, "unsupported dependency-layer command: empty segment")
                if len(words) >= 3 and words[:2] == ["mkdir", "-p"]:
                    for path in words[2:]:
                        (dest / path).mkdir(parents=True, exist_ok=True)
                elif len(words) == 4 and words[0] == "echo" and words[2] == ">":
                    (dest / words[3]).write_text(words[1] + "\n")
                elif words == ["cargo", "fetch", "--locked"]:
                    fetch_seen = True
                else:
                    raise Refusal(
                        f"unsupported dependency-layer command: {words[0]}")
    return fetch_seen


def validate_layer(dest):
    """Every workspace member must resolve from the replayed layer."""
    manifest = tomllib.loads((dest / "Cargo.toml").read_text())
    require((dest / "Cargo.lock").is_file(),
            "Cargo.lock missing from dependency layer")
    members = [".", *manifest["workspace"]["members"]]
    for member in members:
        package = dest / member
        require((package / "Cargo.toml").is_file(),
                f"Missing dependency-layer manifest: {member}")
        require((package / "src/lib.rs").is_file()
                or (package / "src/main.rs").is_file(),
                f"Missing dependency-layer target: {member}")
    return len(members)


def run_locked_fetch(dest):
    """The real locked fetch; raises when the layer does not resolve."""
    try:
        subprocess.run(
            ["cargo", "metadata", "--offline", "--locked", "--no-deps",
             "--format-version", "1", "--manifest-path", str(dest / "Cargo.toml")],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=SUBPROCESS_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise MetadataFetchError(
            f"metadata fetch timeout after {SUBPROCESS_TIMEOUT_SECONDS}s; "
            "next: re-run the check; if the stall repeats, inspect the "
            "cargo metadata invocation and runner load") from error
    except OSError as error:
        raise MetadataFetchError(
            f"metadata fetch spawn failed ({error}); "
            "next: re-run the check on a runner with cargo installed") from error
    except subprocess.CalledProcessError as error:
        raise MetadataFetchError(
            "metadata fetch failed with exit status "
            f"{error.returncode}; next: re-run the check; if it repeats, "
            "inspect the dependency-layer Cargo.toml files") from error


def check_tree(docker_text, root, scratch_dir=None, fetch=run_locked_fetch):
    """Full offline validation of one Dockerfile text against one root."""
    sources = dependency_sources(docker_text)
    require(sources, "no dependency-layer COPY sources found")
    check_sources(sources, root)
    with tempfile.TemporaryDirectory(
            prefix="docker-manifests-", dir=scratch_dir) as directory:
        layer = Path(directory)
        # Called unconditionally: only the boolean goes through require(),
        # so `python -O` still runs the replay instead of stripping it.
        fetch_checked = reconstruct(docker_text, root, layer)
        require(fetch_checked, "dependency fetch layer was not checked")
        validate_layer(layer)
        fetch(layer)


def main():
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") \
        or os.environ.get("RUNNER_TEMP")
    require(scratch,
            "missing scratch directory: set PAPERCLIP_RUN_SCRATCH_DIR "
            "or RUNNER_TEMP to a writable temp directory")
    docker = (ROOT / "Dockerfile").read_text()
    try:
        check_tree(docker, ROOT, scratch_dir=scratch)
    except MetadataFetchError as error:
        print(f"docker-manifests: {error}", file=sys.stderr)
        return 1
    print("Docker dependency-layer manifests: PASS (offline, locked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

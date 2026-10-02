#!/usr/bin/env python3
"""Dependency-aware selector for the container-smoke CI job.

Decides whether a change can alter the runtime image built from
``Dockerfile`` so PRs that touch only docs, worker UI or unrelated CI
tooling skip the ~15-minute amd64 release build. Fail-closed: any
undecidable diff, any unrecognized path, and every non-PR event selects a
full build, so the staging-deploy guardrail (deploy-staging.yml runs on
push to main) always sees a validated image.

The dependency-layer file list is not duplicated here: every Dockerfile
COPY source parsed by ``dockerfile_dependency_sources`` must satisfy
``is_image_input`` (enforced by ``test_container_inputs.py``), keeping
``scripts/check-docker-manifests.py`` / ``scripts/test-docker-deps.py``
the single source of truth for that layer.

Cache note: ``cache-from/to: type=gha,scope=two-bot-runtime-amd64`` is
PR-safe by platform ref-scoping, not by the scope string. A pull_request
run restores the base/main cache but its writes stay on the PR merge ref,
invisible to sibling PRs and to main (verified 2026-10-02: per-ref
``index-two-bot-runtime-amd64`` entries for ``refs/heads/main`` alongside
``refs/pull/*/merge``). Skipped runs additionally write no PR-scoped
cache, reducing one-shot cache bloat.
"""

import argparse
import shlex
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Exact repo-relative files that can change the runtime image.
EXACT_IMAGE_INPUTS = frozenset({
    "Dockerfile",
    ".dockerignore",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "deny.toml",
    "migrations.lock",
    "wrangler/wrangler.toml",  # pins the deployed image reference
})

# Directory prefixes whose contents can change the image. Workflow edits
# change what the image gate means, so they re-validate the image rather
# than risk a silently stale gate.
IMAGE_INPUT_PREFIXES = (
    "crates/",
    "src/",
    "sql/",
    ".cargo/",
    ".github/workflows/",
)

# Only these scripts can change the image or its gate; any other script is
# CI tooling (release, bench, soak) that never enters the image layers. A
# new script is safe to skip: if it affected the image it would be wired
# through the Dockerfile or a workflow, and that wiring file is an input.
IMAGE_INPUT_SCRIPTS = frozenset({
    "scripts/container-smoke.py",
    "scripts/container-inputs.py",
    "scripts/check-docker-manifests.py",
    "scripts/test-docker-deps.py",
    "scripts/test_container_smoke.py",
    "scripts/test_container_inputs.py",
})

# Changes under these prefixes never enter the image: docs, the Worker
# TypeScript UI (the smoke gate tests the Rust binary, not the Worker),
# host systemd units, and the voice-template fixtures (hermetic fixtures
# validated by the check job; not embedded in the binary).
SAFE_SKIP_PREFIXES = (
    "docs/",
    "deploy/",
    "tests/voice_templates/",
    "wrangler/src/",
    "wrangler/test/",
)

# Root meta files that never enter the image layers, plus the repo-level
# GitHub chrome outside `workflows/` (PR template, CODEOWNERS, dependabot
# config): none of it is read by the build or the smoke gate.
SAFE_SKIP_EXACT = frozenset({
    "LICENSE",
    "CHANGELOG.md",
    ".editorconfig",
    ".gitignore",
    ".gitleaks.toml",
    ".gitleaksignore",
    "release-please-config.json",
    ".release-please-manifest.json",
    ".github/pull_request_template.md",
    ".github/CODEOWNERS",
    ".github/dependabot.yml",
})


def dockerfile_dependency_sources(root=ROOT):
    """COPY sources of the Dockerfile dependency layer (pre ``COPY . .``).

    Same parsing as ``scripts/check-docker-manifests.py``: backslash
    continuations joined, ``COPY <src...> <dst>`` split with shlex.
    """
    docker = (root / "Dockerfile").read_text().replace("\\\n", " ")
    sources = []
    for line in docker.splitlines():
        if line.startswith("COPY . ."):
            break
        if line.startswith("COPY "):
            *srcs, _destination = shlex.split(line)[1:]
            sources.extend(srcs)
    return sources


def is_image_input(path):
    """True when a repo-relative path can change the runtime image."""
    # Strip a leading "./" only: lstrip("./") would also eat the dot of
    # dotfiles (".dockerignore" -> "dockerignore") and break exact matches.
    path = path.strip()
    while path.startswith("./"):
        path = path[2:]
    path = path.lstrip("/")
    if path in EXACT_IMAGE_INPUTS:
        return True
    if path.startswith(IMAGE_INPUT_PREFIXES):
        return True
    if path.startswith("scripts/"):
        return path in IMAGE_INPUT_SCRIPTS
    if path.startswith("wrangler/"):
        # Worker UI/tests: never compiled into the Rust binary.
        return False
    if path.startswith("tests/") and not path.startswith("tests/voice_templates/"):
        return True
    if path.startswith(SAFE_SKIP_PREFIXES):
        return False
    if path in SAFE_SKIP_EXACT:
        return False
    # Markdown is never compiled into the image; only workflows control
    # the image gate, so only they re-validate it.
    if Path(path).suffix.lower() == ".md" and not path.startswith(".github/"):
        return False
    # Fail-closed: unrecognized paths (e.g. a brand-new top-level
    # directory) build rather than risk a silently stale image.
    return True


def needs_image_build(changed):
    """True when any changed path needs an image rebuild.

    An empty change list needs no build; anything unrecognized builds
    (fail-closed via :func:`is_image_input`).
    """
    return any(is_image_input(path) for path in changed)


def changed_files(base_ref, head_ref, root=ROOT):
    """Repo-relative paths changed between two refs (two-dot diff)."""
    output = subprocess.run(
        ["git", "-C", str(root), "diff", "--name-only",
         f"{base_ref}..{head_ref}"],
        capture_output=True, text=True, check=True,
    )
    return [line for line in output.stdout.splitlines() if line.strip()]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--head-ref", required=True)
    args = parser.parse_args(argv)
    try:
        changed = changed_files(args.base_ref, args.head_ref)
    except subprocess.CalledProcessError as exc:
        # Undecidable diff (shallow clone, missing ref): fail closed.
        print(f"container-inputs: git diff failed ({exc.returncode}); "
              "selecting a full build", flush=True)
        return 2
    print("true" if needs_image_build(changed) else "false")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

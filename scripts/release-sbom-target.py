"""Bind release inventories to the peeled tag commit; dry-runs never publish."""

import argparse
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys


OVER_CAP = "Release tag response exceeds the byte cap"
DUP_KEYS = "Release tag response carries duplicate object keys"
BAD_CONST = "Release tag response carries a non-finite number"
# Real gh responses are under 1 KiB; the 2 MiB fixture must refuse.
RESPONSE_CAP = 1 << 20


def require_env(name):
    value = os.environ.get(name)
    if value is None:
        raise ValueError(f"{name} is not set")
    return value


def _reject_duplicate_keys(pairs):
    seen = set()
    for key, _ in pairs:
        if key in seen:
            raise ValueError(DUP_KEYS)
        seen.add(key)
    return dict(pairs)


def _reject_constant(value):
    raise ValueError(BAD_CONST)


def _reject_non_finite_float(text):
    number = float(text)
    if not math.isfinite(number):
        raise ValueError(BAD_CONST)
    return number


def parse_response(raw):
    if len(raw.encode("utf-8")) > RESPONSE_CAP:
        raise ValueError(OVER_CAP)
    return json.loads(
        raw,
        object_pairs_hook=_reject_duplicate_keys,
        parse_constant=_reject_constant,
        parse_float=_reject_non_finite_float,
    )


def github_object(path):
    repo = os.environ.get("GH_REPO")
    if repo is None:
        raise ValueError("GH_REPO is not set")
    return parse_response(subprocess.check_output(
        ["gh", "api", f"repos/{repo}/git/{path}"],
        text=True, timeout=30,
    ))


def resolve_tag(tag):
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Publication requires a stable vX.Y.Z release tag")
    # The Git refs API names only tags: a same-named branch cannot win checkout.
    obj = github_object(f"ref/tags/{tag}")["object"]
    for _ in range(8):
        if not re.fullmatch(r"[0-9a-f]{40}", obj.get("sha", "")):
            raise ValueError("Invalid release tag object SHA")
        if obj.get("type") == "commit":
            return obj["sha"]
        if obj.get("type") != "tag":
            raise ValueError("Release tag does not resolve to a commit")
        obj = github_object(f"tags/{obj['sha']}")["object"]
    raise ValueError("Release tag nesting exceeds the resolution bound")


def select_target(dry_run, tag, sha):
    if dry_run == "true":
        if not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise ValueError("Dry-run requires the workflow source SHA")
        return {"ref": sha, "tag": ""}
    return {"ref": resolve_tag(tag), "tag": tag}


def verify_source(tag, expected_sha, source):
    if not re.fullmatch(r"[0-9a-f]{40}", expected_sha) or source.read_text().strip() != expected_sha:
        raise ValueError("SBOM source differs from the selected release commit")
    if resolve_tag(tag) != expected_sha:
        raise ValueError("Release tag moved since SBOM source selection")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-source", type=Path)
    args = parser.parse_args()
    try:
        if args.verify_source:
            verify_source(require_env("RELEASE_TAG"), require_env("EXPECTED_SHA"),
                            args.verify_source)
        else:
            dry_run = os.environ.get("DRY_RUN", "")
            sha = require_env("GITHUB_SHA")
            output_path = require_env("GITHUB_OUTPUT")
            target = select_target(dry_run, os.environ.get("RELEASE_TAG", ""), sha)
            with open(output_path, "a") as output:
                for name, value in target.items():
                    output.write(f"{name}={value}\n")
    except ValueError as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)

"""Bind release inventories to the peeled tag commit; dry-runs never publish."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess


def github_object(path):
    return json.loads(subprocess.check_output(
        ["gh", "api", f"repos/{os.environ['GH_REPO']}/git/{path}"],
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
    if args.verify_source:
        verify_source(os.environ["RELEASE_TAG"], os.environ["EXPECTED_SHA"], args.verify_source)
    else:
        target = select_target(os.environ.get("DRY_RUN", ""), os.environ.get("RELEASE_TAG", ""), os.environ["GITHUB_SHA"])
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            for name, value in target.items():
                output.write(f"{name}={value}\n")

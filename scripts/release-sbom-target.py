"""Select SBOM source without allowing dry-runs to name a publication target."""

import os
import re


def select_target(dry_run, tag, sha):
    if dry_run == "true":
        if not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise ValueError("Dry-run requires the workflow source SHA")
        return {"ref": sha, "tag": ""}
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Publication requires a stable vX.Y.Z release tag")
    return {"ref": tag, "tag": tag}


if __name__ == "__main__":
    target = select_target(os.environ.get("DRY_RUN", ""), os.environ.get("RELEASE_TAG", ""), os.environ["GITHUB_SHA"])
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        for name, value in target.items():
            output.write(f"{name}={value}\n")

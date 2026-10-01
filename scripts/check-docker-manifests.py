#!/usr/bin/env python3
"""Reconstruct Docker's dependency layer and validate it without a Docker daemon."""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
docker = (root / "Dockerfile").read_text().replace("\\\n", " ")
scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ["RUNNER_TEMP"]
with tempfile.TemporaryDirectory(prefix="docker-manifests-", dir=scratch) as directory:
    layer = Path(directory)
    checked = False
    for line in docker.splitlines():
        if line.startswith("COPY . ."):
            break
        if line.startswith("COPY "):
            *sources, destination = shlex.split(line)[1:]
            target = layer / destination
            target.mkdir(parents=True, exist_ok=True)
            for source in sources:
                shutil.copyfile(root / source, target / Path(source).name)
        elif line.startswith("RUN "):
            for command in line[4:].split("&&"):
                words = shlex.split(command)
                if words[:2] == ["mkdir", "-p"]:
                    for path in words[2:]:
                        (layer / path).mkdir(parents=True, exist_ok=True)
                elif words[0] == "echo" and words[2] == ">":
                    (layer / words[3]).write_text(words[1] + "\n")
                elif words == ["cargo", "fetch", "--locked"]:
                    subprocess.run(
                        ["cargo", "metadata", "--offline", "--locked", "--no-deps",
                         "--format-version", "1", "--manifest-path", str(layer / "Cargo.toml")],
                        check=True, stdout=subprocess.DEVNULL,
                    )
                    checked = True
                else:
                    raise AssertionError(f"unsupported dependency-layer command: {words[0]}")
    assert checked, "dependency fetch layer was not checked"
print("Docker dependency-layer manifests: PASS (offline, locked)")

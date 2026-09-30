"""Recreate the Docker dependency layer; --cargo runs its real locked fetch."""

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--cargo", action="store_true")
args = parser.parse_args()

# Test the actual instructions before the real sources are copied, not a
# duplicate list of manifests/stubs that can drift from the Dockerfile.
layer = (ROOT / "Dockerfile").read_text().split("COPY . .", 1)[0]
instructions = layer.replace("\\\n", " ").splitlines()
with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as tmp:
    stage = Path(tmp)
    for instruction in instructions:
        if instruction.startswith("COPY "):
            *sources, destination = shlex.split(instruction)[1:]
            target = stage / destination
            target.mkdir(parents=True, exist_ok=True)
            for source in sources:
                shutil.copy2(ROOT / source, target / Path(source).name)
        elif instruction.startswith("RUN "):
            command = instruction.removeprefix("RUN ")
            assert command.endswith("&& cargo fetch --locked"), command
            if not args.cargo:
                command = command.removesuffix("&& cargo fetch --locked")
            subprocess.run(["sh", "-ec", command], cwd=stage, check=True)

    manifest = tomllib.loads((stage / "Cargo.toml").read_text())
    for member in [".", *manifest["workspace"]["members"]]:
        package = stage / member
        assert (package / "Cargo.toml").is_file(), member
        assert (package / "src/lib.rs").is_file() or (package / "src/main.rs").is_file(), f"Missing dependency-layer target: {member}"
    print(f"PASS Docker dependency layer: {len(manifest['workspace']['members']) + 1} valid package targets" + (", cargo fetch --locked passed" if args.cargo else " (Cargo not run)"))

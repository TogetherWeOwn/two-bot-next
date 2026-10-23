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


class Refusal(Exception):
    """Fixed-vocabulary refusal for Docker dependency-layer validation.

    Every validation path raises this (never ``assert``/``AssertionError``)
    so the verifier fails closed under ``python -O``, where asserts are
    stripped into a silent pass.
    """


def require(condition, message):
    """Raise :class:`Refusal` when condition is false, else return None.

    A function call survives ``python -O``; a bare ``assert`` does not.
    """
    if not condition:
        raise Refusal(message)
    return None


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--cargo", action="store_true")
args = parser.parse_args()

# Dependency layers that must stay in lockstep: the root image and the
# voice-probe experiment image each copy the manifests and stub-build before
# the real sources. Every layer is replayed so the probe copy cannot lag the
# root one until the operator runs the experiment.
DOCKERFILES = ["Dockerfile", "crates/voice-probe/Dockerfile"]

# PASS lines print only after every layer and wiring pin holds: a refusal
# anywhere must leave stdout without PASS, so partial success never reads
# as a pass.
reports = []
for dockerfile in DOCKERFILES:
    # Test the actual instructions before the real sources are copied, not a
    # duplicate list of manifests/stubs that can drift from the Dockerfile.
    layer = (ROOT / dockerfile).read_text().split("COPY . .", 1)[0]
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
                require(command.endswith("&& cargo fetch --locked"), command)
                if not args.cargo:
                    command = command.removesuffix("&& cargo fetch --locked")
                subprocess.run(["sh", "-ec", command], cwd=stage, check=True)

        manifest = tomllib.loads((stage / "Cargo.toml").read_text())
        for member in [".", *manifest["workspace"]["members"]]:
            package = stage / member
            require((package / "Cargo.toml").is_file(), member)
            require((package / "src/lib.rs").is_file() or (package / "src/main.rs").is_file(), f"Missing dependency-layer target: {member}")
        reports.append(f"PASS Docker dependency layer ({dockerfile}): {len(manifest['workspace']['members']) + 1} valid package targets" + (", cargo fetch --locked passed" if args.cargo else " (Cargo not run)"))

# The probe stays a workspace member but outside default-members: the root
# image builds with plain `cargo build --release --locked` (default members
# only), and default `cargo build`/`cargo test` runs must not pull the probe
# targets (including its fixed-port binary test) into every developer run.
workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
members = workspace["members"]
require("crates/voice-probe" in members, "voice-probe must stay a workspace member")
default_members = workspace.get("default-members", members)
require("crates/voice-probe" not in default_members, "voice-probe must stay out of default-members")

for report in reports:
    print(report)

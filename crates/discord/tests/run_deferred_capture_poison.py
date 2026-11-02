"""Run protected buffer acceptance against the real runtime without source edits.

Usage: python3 crates/discord/tests/run_deferred_capture_poison.py [--controls]

The test author may write only tests. These white-box fault tests need a child
module to reach the actual private mutexes; no unsafe layout assumptions or
production injection API is appropriate. Copy the tracked workspace into the
run-owned scratch directory and add ONLY a cfg(test) module declaration there.
The runtime functions themselves are byte-for-byte unchanged. Compile using the
mandatory bounded cache wrapper, with no private target or credential fallback.

Builder must register this same protected module in leveling_runtime.rs so the
ordinary db-feature library CI lane runs it, then retain this offline command.
No intentionally failing acceptance-only PR should be merged independently.
"""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
SOURCE = Path("crates/discord/src/leveling_runtime.rs")
TEST = Path("crates/discord/tests/support/deferred_capture_poison.rs")
MODULE = "deferred_capture_poison_acceptance"
REGISTRATION = (
    '\n#[cfg(test)]\n#[path = "../tests/support/deferred_capture_poison.rs"]\n'
    f"mod {MODULE};\n"
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controls", action="store_true")
    args = parser.parse_args()
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get(
        "PAPERCLIP_SCRATCH_DIR"
    )
    if not scratch:
        parser.error("set a run-owned PAPERCLIP_RUN_SCRATCH_DIR; no /tmp fallback")
    scratch = Path(scratch).resolve(strict=True)
    original = (ROOT / SOURCE).read_bytes()
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
    ).decode().split("\0")
    with tempfile.TemporaryDirectory(prefix="deferred-capture-tests-", dir=scratch) as tmp:
        workspace = Path(tmp)
        for name in filter(None, paths):
            relative = Path(name)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("workspace path escapes the snapshot")
            source = ROOT / relative
            if not source.is_file():
                continue
            destination = workspace / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination, follow_symlinks=False)
        source_text = original.decode()
        # Once Builder registers it, do not register a second copy. The Rust
        # compiler, rather than a source assertion, validates the declaration.
        if f"mod {MODULE};" not in source_text:
            (workspace / SOURCE).write_bytes(original + REGISTRATION.encode())
        test_filter = MODULE + ("::healthy_" if args.controls else "::")
        command = [
            "python3", "scripts/cargo_cache.py", "run", "--", "test",
            "-p", "two-bot-discord", "--features", "db", "--lib", test_filter,
            "--", "--nocapture", "--test-threads=1",
        ]
        print("Runtime SHA-256:", hashlib.sha256(original).hexdigest(), flush=True)
        print("Protected test SHA-256:", hashlib.sha256((ROOT / TEST).read_bytes()).hexdigest(), flush=True)
        print("Scratch registration only; no runtime function changed", flush=True)
        print("Command:", " ".join(command), flush=True)
        result = subprocess.run(command, cwd=workspace, check=False)
    if (ROOT / SOURCE).read_bytes() != original:
        raise RuntimeError("author workspace source changed during verification")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())

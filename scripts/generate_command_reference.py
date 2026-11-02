#!/usr/bin/env python3
"""Explicitly generate the ignored command reference, never tracked configuration.

Run before workspace tests or all-targets Clippy. Use --controller (or
TWO_CARGO_CONTROLLER=1) for bounded Cargo admission on the persistent
controller; nothing selects it implicitly. Successful stdout is
command_reference.v1 JSON. Generation and the read-only reference_docs check
are separate operations: this command does not replace that check.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
EXAMPLE = "generate_command_reference"


def error(code, next_action, retryable=False):
    print(json.dumps({"schema": "command_reference.v1", "status": "error",
                      "code": code, "retryable": retryable, "next_action": next_action}))


def build_executable(cargo, arguments, name, kind):
    try:
        build = subprocess.run(cargo + arguments + ["--locked", "--message-format=json"],
                               cwd=ROOT, stdout=subprocess.PIPE, text=True, check=False)
    except OSError:
        error("cargo_unavailable", "Install the pinned Rust toolchain and retry.")
        return None, 1
    executables = set()
    for line in build.stdout.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            print(line, file=sys.stderr)
            continue
        if not isinstance(record, dict):
            continue
        if record.get("reason") == "compiler-message":
            rendered = record.get("message", {}).get("rendered")
            if rendered:
                print(rendered, end="", file=sys.stderr)
        elif record.get("finding"):
            print(line, file=sys.stderr)
        elif (record.get("reason") == "compiler-artifact"
              and record.get("target", {}).get("name") == name
              and kind in record.get("target", {}).get("kind", [])
              and record.get("executable")):
            executables.add(record["executable"])
    if build.returncode:
        error("cargo_build_failed", "Read the build diagnostics; retry after fixing the failure.",
              retryable=build.returncode == 75)
        return None, build.returncode if build.returncode > 0 else 1
    if len(executables) != 1:
        error("generator_artifact_missing", "Rebuild the pinned generator and test harness; then retry.")
        return None, 1
    return executables.pop(), 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller", action="store_true", help="use the bounded Cargo cache wrapper")
    parser.add_argument("--verify-lifecycle", action="store_true",
                        help="also run independent real-binary mutation tests in an isolated fixture")
    args = parser.parse_args(argv)
    # Explicit opt-in only: other agent runtimes also set PAPERCLIP_RUN_ID but
    # have no bounded cache pool, so that variable must not select controller mode.
    controller = args.controller or os.environ.get("TWO_CARGO_CONTROLLER") == "1"
    cargo = ([sys.executable, str(ROOT / "scripts/cargo_cache.py"), "run", "--"]
             if controller else ["cargo"])
    executable, status = build_executable(
        cargo, ["build", "-p", "two-bot-core", "--example", EXAMPLE], EXAMPLE, "example")
    if status:
        return status
    try:
        # Keep stdout to one final JSON report even when verification is selected.
        generated = subprocess.run([executable], cwd=ROOT, stdout=subprocess.PIPE,
                                   text=True, check=False)
        if generated.returncode:
            print(generated.stdout, end="")
            return generated.returncode if generated.returncode > 0 else 1
        if args.verify_lifecycle:
            reference, status = build_executable(
                cargo, ["test", "-p", "two-bot-core", "--test", "reference_docs", "--no-run"],
                "reference_docs", "test")
            if status:
                return status
            verified = subprocess.run(
                [sys.executable, str(ROOT / "scripts/ci/test_generated_command_reference.py"),
                 "--reference-test-binary", reference, "--generator-binary", executable],
                cwd=ROOT, check=False)
            if verified.returncode:
                error("reference_lifecycle_failed", "Fix the independent lifecycle regression before retrying.")
                return 1
        print(generated.stdout, end="")
        return 0
    except OSError:
        error("generator_unavailable", "Rebuild the generator and test harness; then retry.")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

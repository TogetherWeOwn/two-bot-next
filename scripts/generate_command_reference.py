#!/usr/bin/env python3
"""Explicitly generate the ignored command reference, never tracked configuration.

Run before workspace tests or all-targets Clippy. Use --controller (or
TWO_CARGO_CONTROLLER=1) for bounded Cargo admission on the persistent
controller; the bounded pool policy also selects it implicitly there, while
hosted CI keeps running plain cargo. Successful stdout is
command_reference.v1 JSON. Generation and the read-only reference_docs check
are separate operations: this command does not replace that check.
"""

import argparse
import errno
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
EXAMPLE = "generate_command_reference"

# Bounded-pool policy path shared with scripts/cargo_cache.py (DEFAULT_POOL).
# Present only on the persistent controller; hosted CI runners are ephemeral
# and never carry it.
BOUNDED_POOL_POLICY = Path("/paperclip/.cache/two-bot-next-bounded/policy.json")

# Lookup failures that mean "no policy here", as opposed to a broken lookup.
_POLICY_ABSENT = (errno.ENOENT, errno.ENOTDIR)


def bounded_pool_present():
    """True when the controller's bounded Cargo pool policy exists.

    Raises OSError when the lookup itself fails, so the caller fails closed
    instead of silently falling back to plain cargo.
    """
    try:
        return BOUNDED_POOL_POLICY.is_file()
    except OSError as exc:
        if exc.errno in _POLICY_ABSENT:
            return False
        raise


def controller_selected(explicit_flag):
    """True when the generator build must go through bounded Cargo admission.

    An explicit --controller flag or TWO_CARGO_CONTROLLER=1 always wins, and
    TWO_CARGO_CONTROLLER=0 opts out unless --controller is passed. Otherwise
    the bounded pool policy selects controller mode implicitly, except on
    hosted CI (GITHUB_ACTIONS), which keeps running plain cargo.
    """
    forced = os.environ.get("TWO_CARGO_CONTROLLER")
    if explicit_flag or forced == "1":
        return True
    if forced == "0":
        return False
    if os.environ.get("GITHUB_ACTIONS") == "true":
        return False
    return bounded_pool_present()


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
    # PAPERCLIP_RUN_ID alone never selects controller mode: other agent
    # runtimes set it but have no bounded cache pool.
    try:
        controller = controller_selected(args.controller)
    except OSError:
        error("controller_policy_unreadable",
              "Fix the pool policy permissions, or set TWO_CARGO_CONTROLLER "
              "explicitly; refusing to fall back to plain cargo.")
        return 1
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

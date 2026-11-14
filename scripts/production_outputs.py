#!/usr/bin/env python3
"""Validate a production deploy ID before writing a single GitHub output."""

import argparse
import os
import re
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path

from build_identity import MAX_DEPLOY_ID_LEN, is_build_revision_shape, parse_deploy_id

DEPLOY_ID_SCRIPT = Path(__file__).resolve().with_name("deploy_id.py")
DEPLOY_ID_TIMEOUT = 120
MAX_DEPLOY_ID_STDOUT = MAX_DEPLOY_ID_LEN + 32
DEPLOY_ID_RECORD = re.compile(rb"DEPLOY_ID=([0-9.+a-f]{1,%d})\n?" % MAX_DEPLOY_ID_LEN)
CONTROL = re.compile(r"[\x00-\x1f\x7f-\x9f\u2028\u2029]")


class OutputError(Exception):
    """Fixed diagnostic only; never built from upstream text."""


def _has_control(text):
    return CONTROL.search(text) is not None


def write_output(name, value):
    """Append one validated `name=value` line to GITHUB_OUTPUT."""
    target = os.environ.get("GITHUB_OUTPUT", "")
    if not target or _has_control(target) or not os.path.isabs(target):
        raise OutputError("GITHUB_OUTPUT is not an absolute path")
    if (not re.fullmatch(r"[a-z_]+", name) or not isinstance(value, str)
            or not value or _has_control(value)):
        raise OutputError("output value is not a single safe line")
    try:
        with open(target, "a", encoding="utf-8") as handle:
            handle.write(f"{name}={value}\n")
    except OSError:
        raise OutputError("GITHUB_OUTPUT is not writable") from None


# ---------------------------------------------------------------- deploy-id

def capture_deploy_id(argv):
    """Capture a bounded producer record; never retain producer diagnostics."""
    deadline = time.monotonic() + DEPLOY_ID_TIMEOUT
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                               start_new_session=True)
    try:
        stdout = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise subprocess.TimeoutExpired(argv, DEPLOY_ID_TIMEOUT)
                chunk = os.read(process.stdout.fileno(), MAX_DEPLOY_ID_STDOUT + 1 - len(stdout))
                if not chunk:
                    break
                stdout.extend(chunk)
                if len(stdout) > MAX_DEPLOY_ID_STDOUT:
                    raise OutputError("deploy ID producer output is not exactly one record")
        while not (result := os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(argv, DEPLOY_ID_TIMEOUT)
            time.sleep(min(0.01, remaining))
        if time.monotonic() >= deadline:
            raise subprocess.TimeoutExpired(argv, DEPLOY_ID_TIMEOUT)
        if result.si_code != os.CLD_EXITED or result.si_status != 0:
            raise OutputError("deploy ID producer failed")
        return bytes(stdout)
    finally:
        try:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=1)
        finally:
            process.stdout.close()


def mint_deploy_id(sha):
    """The canonical production deploy ID for `sha`, or OutputError."""
    if not is_build_revision_shape(sha):
        raise OutputError("needs a 40-hex SHA")
    try:
        stdout = capture_deploy_id(
            [sys.executable, str(DEPLOY_ID_SCRIPT), "next",
             "--environment", "production", "--sha", sha]
        )
    except (OSError, subprocess.SubprocessError):
        raise OutputError("deploy ID producer unavailable") from None
    if not isinstance(stdout, bytes) or len(stdout) > MAX_DEPLOY_ID_STDOUT:
        raise OutputError("deploy ID producer output is not exactly one record")
    match = DEPLOY_ID_RECORD.fullmatch(stdout)
    if match is None:
        raise OutputError("deploy ID producer output is not exactly one record")
    value = match.group(1).decode("ascii")
    try:
        _date, _n, sha7 = parse_deploy_id(value)
    except ValueError:
        raise OutputError("deploy ID is not canonical") from None
    if sha7 != sha[:7]:
        raise OutputError("deploy ID names a different commit")
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    mint = commands.add_parser("deploy-id")
    mint.add_argument("--sha", required=True)
    args = parser.parse_args(argv)
    try:
        write_output("deploy_id", mint_deploy_id(args.sha))
    except OutputError as error:
        print(f"production_outputs failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

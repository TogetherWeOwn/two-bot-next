#!/usr/bin/env python3
"""Record one 48-hour watch checkpoint with a read-only readiness poll (stdlib only).

One checkpoint per invocation: poll the production Worker's /readyz once,
check the compiled revision/build-ID match per the production-deploy watch
rows, and emit the checkpoint row (a markdown table row plus a GO/EXTEND
verdict). The probe never writes, migrates, or connects to a database; the
only network call is one GET to the supplied production origin, with no
redirects followed. A ROLLBACK decision stays human: anything short of a
fully ready revision match is EXTEND, never GO.

Usage:
  python3 scripts/cutover_watch_checkpoint.py --checkpoint +15m \\
      --expected-sha <40-hex> --expected-build-id <run>-<attempt> \\
      --production-url https://<production-worker>/

Procedure: docs/cutover-sequence.md section 5 (watch handoff) and
docs/production-deploy.md (48-hour watch log, signal thresholds). The
checkpoint labels below are that template's five rows, in order.
"""

import argparse
import http.client
import json
import os
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))
import production_deploy as deploy_gate  # noqa: E402
import rollback_readiness_probe as probe  # noqa: E402

# Thresholds and wire constants are read from the existing probe/deploy
# config, never redefined here: the component vocabulary and parked set come
# from the M1.2 probe, the SHA/build identity shapes from the production
# deploy gate.
USER_AGENT = probe.USER_AGENT
READYZ_TIMEOUT_SECONDS = probe.READYZ_TIMEOUT_SECONDS
READYZ_BODY_CAP = probe.READYZ_BODY_CAP
READYZ_STATES = probe.READYZ_STATES
READYZ_REQUIRED = probe.READYZ_REQUIRED
READYZ_PARKED = probe.READYZ_PARKED
SHA = deploy_gate.SHA
DIGITS = deploy_gate.DIGITS
UNSTAMPED = deploy_gate.UNSTAMPED

# The five watch-template rows (docs/cutover-sequence.md section 5,
# docs/production-deploy.md 48-hour watch log). This is the first code source
# for the labels; the docs remain authoritative for the procedure.
CHECKPOINTS = ("+15m", "+1h", "+6h", "+24h", "+48h")


class CheckpointError(Exception):
    """A checkpoint's EXTEND reason."""


def production_readyz_url(url):
    """Accept a bare https production origin; return its /readyz endpoint."""
    if not url:
        raise CheckpointError("no production Worker URL given (--production-url or PRODUCTION_WORKER_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise CheckpointError("refusing: not a usable https production origin")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not parts.hostname or parts.path not in ("", "/")
            or parts.query or parts.fragment):
        raise CheckpointError("refusing: not a bare https production origin (no port, "
                              "credentials, path, query or fragment)")
    return f"https://{parts.hostname}/readyz"


def check_build_identity(body, expected_sha, expected_build_id):
    """Match the compiled revision/build ID per the production-deploy watch rows."""
    try:
        report = json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError):
        report = None
    if not isinstance(report, dict):
        raise CheckpointError("/readyz did not return a JSON object (no build identity to match)")
    revision, build_id = report.get("build_revision"), report.get("build_id")
    if not isinstance(revision, str) or not isinstance(build_id, str):
        raise CheckpointError("/readyz has no build_revision and build_id strings")
    if revision != expected_sha:
        if revision == UNSTAMPED:
            raise CheckpointError("build_revision is unknown: this build was not stamped")
        raise CheckpointError(f"build_revision {revision:.40} does not match the deployed SHA")
    if build_id != expected_build_id:
        raise CheckpointError(f"build_id {build_id:.40} is not this run's build "
                              f"({expected_build_id}): a previous container still serves this SHA")
    return f"revision matches the SHA, build {build_id}"


def check_checkpoint(status, body, expected_sha, expected_build_id):
    """Grade one /readyz answer: GO only on 200 + all-ready + identity match."""
    state = probe.readyz_state(status, body)
    identity = check_build_identity(body, expected_sha, expected_build_id)
    not_ready = sorted((name, value) for name, value in state.items() if value != "ready")
    if status == 200 and not not_ready:
        return f"/readyz 200: all {len(state)} components ready, {identity}"
    if not_ready:
        shown = ", ".join(f"{name} {value}" for name, value in not_ready)
        parked = all(value in READYZ_PARKED.get(name, ()) for name, value in not_ready)
        if status == 503 and parked:
            raise CheckpointError(f"/readyz 503: process ready, {shown} "
                                  f"(parked, not acceptance), {identity}")
        raise CheckpointError(f"/readyz {status} with unexpected components: {shown:.200} ({identity})")
    raise CheckpointError(f"/readyz {status} with all components ready ({identity}): "
                          "expected 200 for GO")


def valid_sha(value):
    if not SHA.fullmatch(value):
        raise argparse.ArgumentTypeError("expected-sha must be a 40-character hex commit")
    return value


def valid_build_id(value):
    parts = value.split("-")
    if len(parts) != 2 or not all(DIGITS.fullmatch(p) for p in parts):
        raise argparse.ArgumentTypeError("expected-build-id must be <run-id>-<attempt> (digits-digits)")
    return value


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--checkpoint", required=True, choices=CHECKPOINTS,
                        help="watch-template row to record (docs/production-deploy.md 48-hour watch log)")
    parser.add_argument("--expected-sha", required=True, type=valid_sha,
                        help="deployed 40-hex commit the compiled build_revision must equal")
    parser.add_argument("--expected-build-id", required=True, type=valid_build_id,
                        help="this run's <run-id>-<attempt> the compiled build_id must equal")
    parser.add_argument("--production-url", default=os.environ.get("PRODUCTION_WORKER_URL"),
                        help="production Worker origin (default: $PRODUCTION_WORKER_URL)")
    return parser.parse_args(argv)


def run(args, fetch_fn=None):
    """Poll once and grade the checkpoint. Returns (verdict, row)."""
    fetch_one = fetch_fn if fetch_fn is not None else probe.fetch
    try:
        endpoint = production_readyz_url(args.production_url)
    except CheckpointError as e:
        raise CheckpointError(str(e))
    try:
        status, body = fetch_one(endpoint)
    except (OSError, http.client.HTTPException) as e:
        raise CheckpointError(f"{endpoint} did not respond ({e.__class__.__name__})")
    observation = check_checkpoint(status, body, args.expected_sha, args.expected_build_id)
    return observation


def main(argv=None, fetch_fn=None):
    args = parse_args(argv)
    utc = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    try:
        observation = run(args, fetch_fn)
    except (CheckpointError, probe.ProbeError) as e:
        row = (f"| {utc} | {args.checkpoint} checkpoint | `readyz` / `revision` "
               f"| {e} | EXTEND |")
        print(row)
        return 1
    row = (f"| {utc} | {args.checkpoint} checkpoint | `readyz` / `revision` "
           f"| {observation} | GO |")
    print(row)
    return 0


if __name__ == "__main__":
    sys.exit(main())

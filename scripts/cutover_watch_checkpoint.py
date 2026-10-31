#!/usr/bin/env python3
"""Record one 48-hour watch checkpoint with a read-only readiness poll (stdlib only).

One checkpoint per invocation: poll the production Worker's /readyz once,
check the compiled revision/build-ID match per the production-deploy watch
rows, and emit the checkpoint row (a markdown table row plus a GO/EXTEND
verdict, or a one-line JSON receipt with --format json). The probe never writes,
migrates, or connects to a database; the only network call is one GET to the
supplied production origin, with no redirects followed. A ROLLBACK decision
stays human: anything short of a fully ready revision match is EXTEND, never GO.

Only the canonical production workers.dev origin is admitted (the same
allowlist the passive production probe enforces); staging, arbitrary hosts
and malformed origins are refused before any fetch, with zero requests sent.

Usage:
  python3 scripts/cutover_watch_checkpoint.py --checkpoint +15m \\
      --expected-sha <40-hex> --expected-build-id <run>-<attempt> \\
      --production-url https://<production-worker>/ [--format markdown|json]

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
import prod_passive_probe as prod_probe
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

# The passive production probe owns the production-origin allowlist; the
# checkpoint reuses its objects so it can never drift into a second policy.
PROD_HOST = prod_probe.PROD_HOST
STAGING_LABEL = prod_probe.STAGING_LABEL

# The five watch-template rows (docs/cutover-sequence.md section 5,
# docs/production-deploy.md 48-hour watch log). This is the first code source
# for the labels; the docs remain authoritative for the procedure.
CHECKPOINTS = ("+15m", "+1h", "+6h", "+24h", "+48h")

# Operator next step per stable reason id. Fixed vocabulary only: receipts
# never echo unvalidated input.
NEXT_ACTION = {
    "ready_match": "paste the GO row onto the execution card and continue the watch",
    "missing_origin": "supply --production-url (or $PRODUCTION_WORKER_URL) "
                      "with the production Worker origin",
    "staging_refused": "point --production-url at the production Worker origin; "
                       "staging has its own probe",
    "origin_not_admitted": "point --production-url at the production workers.dev origin",
    "fetch_failed": "re-poll; extend the watch if /readyz stays unreachable",
    "duplicate_keys": "extend the watch; require a clean /readyz body before GO",
    "identity_unstamped": "extend the watch; require a stamped build before GO",
    "identity_shape_unexpected": "extend the watch; require valid build identity before GO",
    "revision_mismatch": "extend the watch; a previous container still serves this origin",
    "build_mismatch": "extend the watch; a previous container still serves this SHA",
    "components_not_ready": "extend the watch; require every component ready before GO",
    "readyz_unexpected": "extend the watch; require a shape-correct /readyz before GO",
}


class CheckpointError(Exception):
    """A checkpoint's EXTEND reason, with a stable snake_case code."""

    def __init__(self, message, code=None):
        super().__init__(message)
        self.code = code


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def decode_strict(body):
    """Decode JSON while refusing duplicate object keys at any depth."""
    return json.loads(body, object_pairs_hook=_reject_duplicate_keys)


def production_readyz_url(url):
    """Admit only the canonical production origin; return its /readyz endpoint."""
    if not url:
        raise CheckpointError("no production Worker URL given (--production-url or PRODUCTION_WORKER_URL)",
                              code="missing_origin")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise CheckpointError("refusing: not the two-bot-next production workers.dev origin",
                              code="origin_not_admitted")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    host = parts.hostname or ""
    if STAGING_LABEL in host:
        raise CheckpointError("refusing staging host on the production watch checkpoint",
                              code="staging_refused")
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not PROD_HOST.fullmatch(host)
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise CheckpointError("refusing: not the two-bot-next production workers.dev origin",
                              code="origin_not_admitted")
    return f"https://{host}/readyz"


def is_build_id_shape(value):
    """Digits-digits run shape, shared by CLI validation and remote checks."""
    parts = value.split("-")
    return len(parts) == 2 and all(DIGITS.fullmatch(p) for p in parts)


def check_build_identity(body, expected_sha, expected_build_id):
    """Match the compiled revision/build ID per the production-deploy watch rows.

    Remote identity shapes are validated before any value enters diagnostics:
    only the unstamped marker, a 40-char hex revision, or a digits-digits
    build ID can appear in output. Anything else is a fixed EXTEND reason, so
    a newline, pipe, or secret-shaped remote string can neither break the
    single checkpoint row nor leak into evidence.
    """
    try:
        report = decode_strict(body)
    except DuplicateKeyError:
        raise CheckpointError("/readyz carries duplicate JSON object keys",
                              code="duplicate_keys")
    except (ValueError, RecursionError):
        report = None
    if not isinstance(report, dict):
        raise CheckpointError("/readyz did not return a JSON object (no build identity to match)",
                              code="readyz_unexpected")
    revision, build_id = report.get("build_revision"), report.get("build_id")
    if not isinstance(revision, str) or not isinstance(build_id, str):
        raise CheckpointError("/readyz has no build_revision and build_id strings",
                              code="identity_shape_unexpected")
    if revision == UNSTAMPED:
        raise CheckpointError("build_revision is unknown: this build was not stamped",
                              code="identity_unstamped")
    if not SHA.fullmatch(revision):
        raise CheckpointError("build_revision has an unexpected shape: "
                              "expected a 40-character hex commit",
                              code="identity_shape_unexpected")
    if revision != expected_sha:
        raise CheckpointError(f"build_revision {revision} does not match the deployed SHA",
                              code="revision_mismatch")
    if not is_build_id_shape(build_id):
        raise CheckpointError("build_id has an unexpected shape: "
                              "expected <run-id>-<attempt> (digits-digits)",
                              code="identity_shape_unexpected")
    if build_id != expected_build_id:
        raise CheckpointError(f"build_id {build_id} is not this run's build "
                              f"({expected_build_id}): a previous container still serves this SHA",
                              code="build_mismatch")
    return f"revision matches the SHA, build {expected_build_id}"


def observed_identity(body):
    """The validated stamped identity from a /readyz body, or None.

    Only a 40-char hex revision paired with a digits-digits build ID is
    returned; anything unvalidated (duplicates, non-JSON, unstamped or
    malformed identity) yields None so receipts never echo it.
    """
    try:
        report = decode_strict(body)
    except (ValueError, RecursionError):
        return None
    if not isinstance(report, dict):
        return None
    revision, build_id = report.get("build_revision"), report.get("build_id")
    if (isinstance(revision, str) and isinstance(build_id, str)
            and SHA.fullmatch(revision) and is_build_id_shape(build_id)):
        return {"build_revision": revision, "build_id": build_id}
    return None


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
                                  f"(parked, not acceptance), {identity}",
                                  code="components_not_ready")
        raise CheckpointError(f"/readyz {status} with unexpected components: {shown:.200} ({identity})",
                              code="components_not_ready")
    raise CheckpointError(f"/readyz {status} with all components ready ({identity}): "
                          "expected 200 for GO",
                          code="readyz_unexpected")


def valid_sha(value):
    if not SHA.fullmatch(value):
        raise argparse.ArgumentTypeError("expected-sha must be a 40-character hex commit")
    return value


def valid_build_id(value):
    if not is_build_id_shape(value):
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
    parser.add_argument("--format", default="markdown", choices=("markdown", "json"),
                        help="output shape: a markdown table row (default) or a one-line JSON receipt")
    return parser.parse_args(argv)


def _receipt(args, verdict, reason, detail, origin, expected, observed):
    return {"checkpoint": args.checkpoint,
            "verdict": verdict,
            "reason": reason,
            "reason_detail": detail,
            "origin": origin,
            "expected": expected,
            "observed": observed,
            "next_action": NEXT_ACTION[reason]}


def run(args, fetch_fn=None):
    """Poll once and grade the checkpoint. Returns a receipt dict."""
    fetch_one = fetch_fn if fetch_fn is not None else probe.fetch
    expected = {"build_revision": args.expected_sha, "build_id": args.expected_build_id}
    try:
        endpoint = production_readyz_url(args.production_url)
    except CheckpointError as error:
        return _receipt(args, "EXTEND", error.code, str(error), None, expected, None)
    origin = endpoint[: -len("/readyz")]
    try:
        status, body = fetch_one(endpoint)
    except (OSError, http.client.HTTPException) as error:
        detail = f"{endpoint} did not respond ({error.__class__.__name__})"
        return _receipt(args, "EXTEND", "fetch_failed", detail, origin, expected, None)
    observed = observed_identity(body)
    try:
        observation = check_checkpoint(status, body, args.expected_sha, args.expected_build_id)
    except CheckpointError as error:
        return _receipt(args, "EXTEND", error.code or "readyz_unexpected",
                        str(error), origin, expected, observed)
    except probe.ProbeError as error:
        # readyz_state converts strict-decode duplicates into a ProbeError
        # whose "duplicate JSON object key" vocabulary is pinned by both
        # modules' tests; classify on it, everything else is unexpected.
        detail = str(error)
        reason = "duplicate_keys" if "duplicate JSON object key" in detail else "readyz_unexpected"
        return _receipt(args, "EXTEND", reason, detail, origin, expected, observed)
    return _receipt(args, "GO", "ready_match", observation, origin, expected, observed)


def main(argv=None, fetch_fn=None):
    args = parse_args(argv)
    utc = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    receipt = run(args, fetch_fn)
    if args.format == "json":
        print(json.dumps(receipt))
        return 0 if receipt["verdict"] == "GO" else 1
    detail = receipt["reason_detail"]
    if receipt["origin"] is None:
        detail = f"{detail} [{receipt['reason']}]"
    else:
        expected, observed = receipt["expected"], receipt["observed"]
        detail += (f" [origin {receipt['origin']} "
                   f"expected {expected['build_revision']}/{expected['build_id']}")
        if observed is not None:
            detail += f" observed {observed['build_revision']}/{observed['build_id']}"
        detail += "]"
    row = (f"| {utc} | {args.checkpoint} checkpoint | `readyz` / `revision` "
           f"| {detail} | {receipt['verdict']} |")
    print(row)
    return 0 if receipt["verdict"] == "GO" else 1


if __name__ == "__main__":
    sys.exit(main())

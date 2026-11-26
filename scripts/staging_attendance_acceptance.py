#!/usr/bin/env python3
"""Probe staged signed attendance or compare one rehearsal's offline readbacks."""

import argparse
from datetime import datetime, timedelta, timezone
import hashlib
import hmac
import http.client
import json
import os
import re
import secrets
import signal
import sys
import time
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request

from build_identity import is_build_id_shape, is_build_revision_shape, is_deploy_id_shape
from script_intake import decode_json, no_redirect_opener, read_with_overflow
from staging_automation_read_smoke import SmokeError, guild_fence
from staging_health_contract_probe import BODY_CAP, ProbeError, parse_readyz
from staging_rollout import UUID, readiness_identity

ACTION_PATH = "/internal/actions"
ACTION = "event.attendance.read"
STAGING_ORIGIN = "https://two-bot-next-staging.5150.workers.dev"
HTTP_HEADERS = {"User-Agent": "two-bot-staging-attendance-acceptance/1.0", "Cache-Control": "no-cache"}
MEMBER = "300000000000000001"
ROOM = "400000000000000001"
CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
PHASES = ("pending", "finalized", "off", "cleanup")
READY_TIMEOUT = 5
WEB_TIMEOUT = 2.5
RUN_BUDGET = 25


class Refused(Exception):
    """Fixed vocabulary. Never include remote bodies or credential values."""


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def json_object(raw):
    require(len(raw) <= BODY_CAP, "body too large")
    value = decode_json(raw.decode("utf-8"), max_depth=16)
    require(isinstance(value, dict), "expected JSON object")
    return value


def read_object(path):
    with path.open("rb") as stream:
        return json_object(read_with_overflow(stream, BODY_CAP))


def validate_probe_window(lifecycle, *, recorded=False):
    use = lifecycle["use"]
    observed_at = lifecycle.get("observedAt")
    require(isinstance(observed_at, str) and re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z", observed_at),
        "staging lease did not cover the probe")
    since_epoch = datetime.fromisoformat(observed_at) - datetime(1970, 1, 1, tzinfo=timezone.utc)
    observed_ms = since_epoch // timedelta(milliseconds=1)
    start_ms = observed_ms if recorded else time.time() * 1000
    require(type(use.get("acquiredAt")) is int and 0 <= use["acquiredAt"] <= observed_ms <= start_ms
            and type(use.get("expiresAt")) is int and start_ms + RUN_BUDGET * 1000 < use["expiresAt"],
            "staging lease did not cover the probe")


def validate_configuration(phase, origin, rollout, observed, *, recorded=False):
    version, sha, build, deploy = (rollout.get(name) for name in
                                   ("worker_version", "revision", "build_id", "deploy_id"))
    require(isinstance(version, str) and re.fullmatch(UUID, version)
            and is_build_revision_shape(sha) and is_build_id_shape(build)
            and is_deploy_id_shape(deploy) and deploy.endswith("+" + sha[:7])
            and rollout.get("readyz") == 200 and rollout.get("health") == 200,
            "invalid staging rollout identity")
    guild_fence(observed.get("guild_id"))
    require(observed.get("environment") == "staging" and observed.get("origin") == origin
            and observed.get("worker_version") == version and observed.get("build_revision") == sha
            and observed.get("build_id") == build and observed.get("deploy_id") == deploy,
            "configuration is for a different staging build")
    active_read = phase in ("pending", "finalized")
    require(observed.get("attendance_read") is active_read
            and observed.get("attendance_capture") is False,
            "attendance flags differ from the phase")
    lifecycle = observed.get("lifecycle")
    require(isinstance(lifecycle, dict) and lifecycle.get("schema") == "two-bot-staging-use.v1"
            and lifecycle.get("environment") == "staging" and lifecycle.get("deploymentId") == version
            and lifecycle.get("sha") == sha, "staging lifecycle identity did not match")
    use = lifecycle.get("use")
    require(isinstance(use, dict) and use.get("sha") == sha and use.get("deploymentId") == version
            and type(use.get("epoch")) is int and use["epoch"] > 0
            and use["epoch"] == lifecycle.get("epoch"), "staging lease identity did not match")
    if phase == "cleanup":
        require(lifecycle.get("action") == "release" and use.get("phase") == "released" and lifecycle.get("running") is False
                and type(observed.get("fixture_rows")) is int
                and observed["fixture_rows"] == 0, "cleanup readback is incomplete")
    else:
        require(lifecycle.get("action") == "ready" and lifecycle.get("running") is True
                and use.get("phase") == "active",
                "staging lease did not cover the probe")
        validate_probe_window(lifecycle, recorded=recorded)
        expected_fixture = "pending" if phase == "pending" else "finalized"
        require(observed.get("signed_ingress") is True and observed.get("primary_fixture") == expected_fixture
                and (phase != "finalized" or observed.get("foreign_fixture") == "foreign-only"),
                "signed ingress or synthetic fixture readback is incomplete")
    return version, sha, build, deploy


def validate_rehearsal(rollout, observations):
    require(set(observations) == set(PHASES) and all(isinstance(row, dict) for row in observations.values()),
            "rehearsal requires all four observations")
    original_lease = None
    for phase in PHASES:
        observed = observations[phase]
        validate_configuration(phase, STAGING_ORIGIN, rollout, observed, recorded=True)
        lifecycle = observed["lifecycle"]
        use = lifecycle["use"]
        run_id, attempt, acquired, expires = (use.get(field) for field in
                                             ("runId", "runAttempt", "acquiredAt", "expiresAt"))
        require(use.get("environment") == "staging" and type(lifecycle.get("epoch")) is int
                and isinstance(run_id, str) and re.fullmatch(r"[1-9][0-9]{0,19}", run_id)
                and type(attempt) is int and attempt > 0
                and type(acquired) is int and type(expires) is int and 0 <= acquired < expires,
                "staging lease identity did not match")
        # The owner epoch can remain unchanged when a second window is acquired.
        lease = (use["epoch"], run_id, attempt, acquired, expires)
        if phase == "pending":
            original_lease = lease
        require(lease == original_lease, "staging lease identity did not match")


def fixture_result(event_key, start):
    def stamp(moment):
        return moment.isoformat(timespec="milliseconds").replace("+00:00", "Z")

    return {"v": 1, "event_key": event_key, "final": True, "threshold_minutes": 10,
            "peak_concurrent": 1,
            "window": {"starts_at": stamp(start), "ends_at": stamp(start + timedelta(minutes=30))},
            "rooms": [{"channel_id": ROOM, "name_at_close": "Synthetic attendance room"}],
            "members": [{"discord_id": MEMBER, "minutes": 12, "sessions": 1,
                         "first_joined_at": stamp(start + timedelta(minutes=1))}]}


def fixture_start(value):
    require(isinstance(value, str) and re.fullmatch(r"20[0-9]{2}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:00Z", value),
            "invalid synthetic fixture start")
    return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def signing_headers(key_id, secret, timestamp, nonce, body):
    # Same byte-for-byte request contract as core internal_actions::sign.
    digest = hashlib.sha256(body).hexdigest()
    canonical = f"POST\n{ACTION_PATH}\n{timestamp}\n{nonce}\n{digest}".encode("ascii")
    signature = hmac.digest(secret, canonical, "sha256").hex()
    return {"x-two-key-id": key_id, "x-two-timestamp": timestamp, "x-two-nonce": nonce,
            "x-two-signature": "sha256=" + signature}


def credentials(env):
    key_id = env.get("STAGING_ATTENDANCE_KEY_ID", "")
    key = env.get("STAGING_ATTENDANCE_SIGNING_SECRET", "")
    require(re.fullmatch(r"[A-Za-z0-9._-]{1,128}", key_id)
            and re.fullmatch(r"[0-9a-f]{64}", key), "missing dedicated staging signing credentials")
    return key_id, key.encode("ascii")


def send(request):
    opener = no_redirect_opener(use_proxy=False)
    try:
        response = opener.open(request, timeout=READY_TIMEOUT if request.data is None else WEB_TIMEOUT)
    except HTTPError as error:
        response = error
    with response:
        headers = response.headers
        require(not 300 <= response.code < 400 and not headers.get_all("location")
                and not headers.get_all("content-encoding"),
                "redirect or encoded response refused")
        media = headers.get_all("content-type", [])
        require(len(media) == 1 and media[0].split(";")[0].strip().lower() == "application/json",
                "expected application/json response")
        for single in ("x-two-worker-version", "cache-control"):
            require(len(headers.get_all(single, [])) <= 1, "ambiguous response headers")
        return response.code, headers, json_object(read_with_overflow(response, BODY_CAP))


def ready(check, origin, identity):
    status, headers, result = check(Request(origin + "/readyz", headers=HTTP_HEADERS))
    require(status == 200 and readiness_identity(headers, result, *identity) == "match",
            "serving build did not match the staging rollout")
    state, report = parse_readyz(status, result)
    require(all(part == "ready" for part in state.values()) and report.get("gateway_failure") is None,
            "staging Bot is not ready")


def signed_read(check, origin, signer, event_key, expected_status, expected):
    body = json.dumps({"action": ACTION, "event_key": event_key}, separators=(",", ":")).encode("ascii")
    request_id = "".join(secrets.choice(CROCKFORD) for _ in range(26))
    headers = signing_headers(*signer, str(int(time.time())), secrets.token_hex(16), body)
    headers.update({"Content-Type": "application/json", "x-request-id": request_id})
    started = time.monotonic()
    status, response_headers, result = check(Request(origin + ACTION_PATH, data=body, headers={**HTTP_HEADERS, **headers}))
    require(time.monotonic() - started < WEB_TIMEOUT, "signed read exceeded the Web deadline")
    envelope = {"ok": expected_status == 200, "request_id": request_id, **expected}
    # Canonical JSON equality preserves JSON types (Python bool/int equality does not).
    require(status == expected_status and response_headers.get("cache-control") == "no-store"
            and json.dumps(result, sort_keys=True) == json.dumps(envelope, sort_keys=True),
            "signed attendance response did not match the expected envelope")


def probe(phase, origin, identity, start, signer, check):
    event_key = f"attendance-acceptance:{identity[2]}"
    not_found = {"error": {"code": "not_found", "message": "request refused", "retryable": False}}
    denied = {"error": {"code": "action_not_allowed", "message": "request refused", "retryable": False}}
    if phase == "finalized":
        cases = [(event_key, 200, {"result": fixture_result(event_key, start)}),
                 (event_key + ":foreign", 404, not_found)]
    elif phase == "pending":
        cases = [(event_key, 404, not_found)]
    else:
        cases = [(event_key, 403, denied)]
    ready(check, origin, identity)
    for key, status, expected in cases:
        signed_read(check, origin, signer, key, status, expected)
        ready(check, origin, identity)


def deadline(_signum, _frame):
    raise Refused("staging probe deadline elapsed")


def main(argv=None, *, env=None, check=send):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=(*PHASES, "rehearsal"))
    parser.add_argument("--rollout", required=True, type=Path)
    parser.add_argument("--observed", required=True, type=Path, help="phase observation or rehearsal object keyed by phase")
    parser.add_argument("--fixture-start")
    parser.add_argument("--execute", action="store_true", help="enable signed HTTPS reads")
    args = parser.parse_args(argv)
    try:
        if args.phase == "rehearsal":
            require(not args.execute and args.fixture_start is None, "rehearsal only accepts offline readback")
            validate_rehearsal(read_object(args.rollout), read_object(args.observed))
        else:
            rollout, observed = read_object(args.rollout), read_object(args.observed)
            identity = validate_configuration(args.phase, STAGING_ORIGIN, rollout, observed)
            if args.phase == "cleanup":
                require(not args.execute and args.fixture_start is None, "cleanup only accepts offline readback")
            elif not args.execute:
                print("DRY RUN: attendance configuration matched, no signed check performed")
                return 2
            else:
                start = fixture_start(args.fixture_start) if args.phase == "finalized" else None
                signer = credentials(os.environ if env is None else env)
                old_handler = signal.signal(signal.SIGALRM, deadline)
                try:
                    signal.alarm(RUN_BUDGET)
                    # Use a wall-clock sample after arming to cover the whole alarm.
                    validate_probe_window(observed["lifecycle"])
                    probe(args.phase, STAGING_ORIGIN, identity, start, signer, check)
                finally:
                    signal.alarm(0)
                    signal.signal(signal.SIGALRM, old_handler)
    except (Refused, OSError, ValueError, RecursionError, http.client.HTTPException, ProbeError, SmokeError) as error:
        reason = str(error) if isinstance(error, Refused) else "attendance acceptance input or transport refused"
        print(f"FAIL: {reason}", file=sys.stderr)
        return 1
    print(f"PASS: attendance {args.phase}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

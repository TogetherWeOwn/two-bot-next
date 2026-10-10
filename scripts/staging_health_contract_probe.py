#!/usr/bin/env python3
"""Read-only staging health-contract probe for the bot container (stdlib only).

Two GETs against the staging Worker origin, each traced to the Rust contract
(`crates/bot/src/server.rs`, `crates/core/src/health.rs`):

  health     GET /health answers 200 {"status":"ok"}. Process liveness only:
             always 200 while the process answers, never a readiness claim.
  readyz     GET /readyz answers 200 or 503 with the readiness breakdown:
             components (process/gateway/database/token_invalid, each
             ready/starting/down), informational jobs, an optional
             gateway_failure {phase, class} in the fixed vocabulary, and the
             compiled build_revision/build_id.

The probe asserts the shape, the status-code mapping (200 if and only if
every component is ready), and the build-identity fields. A gateway_failure
class of checkpoint_load_failed is reported as the DB-behind-binary
(migration-lag) signature, not a generic gateway outage
(docs/voice-cutover-rollback-triggers.md T1). A shape-correct 503 is still a
FAIL: a truthful parked process is never E2E approval.

The probe never writes, never touches production, follows no redirects, and
sends no credentials. The only network calls are the two GETs above.

Body-cap refusal contract: the transport reads at most BODY_CAP+1 bytes on
both the normal and HTTPError paths so a hidden suffix is detected. Any body
longer than BODY_CAP (64 KiB) is refused before JSON parsing with the fixed
message "<name> probe answered over the body cap"; exact-cap valid input
passes, and the extra byte never reaches a successful JSON path. Refusals
never echo the body, headers, URL or parse exceptions.

Origin fence (fail-closed, before any request): only an
https://two-bot-next-staging.<sub>.workers.dev origin (no port, userinfo,
path, query or fragment) is fetched; anything else -- including the
production Worker -- refuses with exit 2 and no request is sent.

Usage:
  STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \\
      python3 scripts/staging_health_contract_probe.py [--evidence FILE] [--expected-sha SHA]
"""

import argparse
from dataclasses import dataclass
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
BODY_CAP = 64 << 10
TIMEOUT_SECONDS = 10
USER_AGENT = "two-bot-next-staging-health-contract-probe/1.0 (read-only E2E)"

REQUIRED_COMPONENTS = ("process", "gateway", "database", "token_invalid")
STATUSES = ("ready", "starting", "down")
FAILURE_PHASE = "durable_gateway"
FAILURE_CLASSES = frozenset({
    "store_unavailable",
    "gateway_pool_connect_failed",
    "checkpoint_load_failed",
    "onboarding_gates_invalid",
    "onboarding_init_failed",
    "custom_commands_init_failed",
    "milestones_load_failed",
    "automod_config_invalid",
    "automod_executor_failed",
    "raid_executor_failed",
    "gateway_runtime_failed",
    "gateway_task_panicked",
})
# The staging rollout-timeout lesson: the database lags the binary's
# migration set, boot reads fail, the container crashloops. Surfaced here
# as gateway_failure durable_gateway:checkpoint_load_failed.
DB_BEHIND_CLASS = "checkpoint_load_failed"


class ProbeError(Exception):
    """A check's one-line failure reason (fixed vocabulary plus observed values)."""


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason}"


def staging_origin(url):
    """Allowlist the staging Worker origin; refuse everything else unread."""
    if not url:
        raise ProbeError("no staging Worker URL given (--staging-url or STAGING_WORKER_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise ProbeError("refusing: not the two-bot-next-staging workers.dev origin")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not STAGING_HOST.fullmatch(parts.hostname or "")
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise ProbeError("refusing: not the two-bot-next-staging workers.dev origin")
    return f"https://{parts.hostname}"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


def fetch(url):
    """One GET without redirects; returns (status, headers, body bytes)."""
    opener = urllib.request.build_opener(_NoRedirect)
    request = urllib.request.Request(url, method="GET",
                                     headers={"User-Agent": USER_AGENT,
                                              "Cache-Control": "no-cache"})
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            return response.status, response.headers, response.read(BODY_CAP + 1)
    except urllib.error.HTTPError as error:
        return error.code, error.headers or {}, error.read(BODY_CAP + 1)


def header(headers, name):
    """Case-insensitive single header lookup; headers stay server-side."""
    if headers is None:
        return None
    get = getattr(headers, "get", None)
    if callable(get):
        try:
            return get(name)
        except (AttributeError, TypeError, ValueError):
            return None
    if isinstance(headers, dict):
        for key, value in headers.items():
            if isinstance(key, str) and key.lower() == name:
                return value if isinstance(value, str) else None
    return None


def get_json(fetch_fn, url, name):
    """One GET returning (status, headers, parsed JSON or None)."""
    try:
        status, headers, body = fetch_fn(url)
    except (OSError, http.client.HTTPException) as error:
        raise ProbeError(f"{name} probe did not respond ({error.__class__.__name__})")
    if len(body) > BODY_CAP:
        raise ProbeError(f"{name} probe answered over the body cap")
    try:
        value = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        value = None
    return status, headers, value


def check_health(origin, fetch_fn):
    status, headers, body = get_json(fetch_fn, origin + "/health", "health")
    if status != 200:
        raise ProbeError(f"health answered {status}, expected the exact 200 liveness")
    if body != {"status": "ok"}:
        raise ProbeError("health 200 without the exact status shape")
    if header(headers, "location") is not None:
        raise ProbeError("health 200 carries a redirect target")
    return "health 200 with the exact liveness shape"


def parse_readyz(status, value):
    """Validate the readiness breakdown; return (state dict, report)."""
    if not isinstance(value, dict) or value.get("error") is not None:
        raise ProbeError(f"readyz {status} without the bot's component breakdown "
                         "(ownership refusal or broken deploy)")
    components = value.get("components")
    if (not isinstance(components, list) or not components
            or not all(isinstance(row, list) and len(row) == 2
                       and isinstance(row[0], str) and row[0]
                       and row[1] in STATUSES for row in components)):
        raise ProbeError(f"readyz {status} without the bot's component breakdown "
                         "(ownership refusal or broken deploy)")
    state = dict(components)
    missing = [name for name in REQUIRED_COMPONENTS if name not in state]
    if missing:
        raise ProbeError(f"readyz {status} without the bot's component breakdown "
                         f"(missing {', '.join(missing)})")
    jobs = value.get("jobs")
    if not isinstance(jobs, dict):
        raise ProbeError(f"readyz {status} without the informational jobs object")
    failure = value.get("gateway_failure")
    if failure is not None:
        if (not isinstance(failure, dict) or failure.get("phase") != FAILURE_PHASE
                or failure.get("class") not in FAILURE_CLASSES):
            raise ProbeError(f"readyz {status} with a gateway failure outside the fixed vocabulary")
    return state, value


def check_build(report, expected_sha):
    if not isinstance(report, dict):
        return Result("build", False, "no readyz body to read build identity from")
    revision = report.get("build_revision")
    build_id = report.get("build_id")
    if not isinstance(revision, str) or not revision or not isinstance(build_id, str) or not build_id:
        return Result("build", False, "readyz without the compiled build_revision/build_id")
    if expected_sha and revision != expected_sha:
        return Result("build", False,
                      f"build_revision {revision} does not match the expected SHA")
    detail = f"build_revision {revision} build_id {build_id}"
    if expected_sha:
        detail += " matches the expected SHA"
    return Result("build", True, detail)


def run(args, fetch_fn=fetch):
    try:
        origin = staging_origin(args.staging_url)
    except ProbeError as error:
        return None, [Result("origin", False, str(error))], None
    results = []
    try:
        results.append(Result("health", True, check_health(origin, fetch_fn)))
    except ProbeError as error:
        results.append(Result("health", False, str(error)))
    report = None
    try:
        status, _, value = get_json(fetch_fn, origin + "/readyz", "readyz")
        state, report = parse_readyz(status, value)
        expected = 200 if all(text == "ready" for text in state.values()) else 503
        if status != expected:
            raise ProbeError(f"readyz {status} contradicts the component breakdown "
                             f"(expected {expected})")
        if status == 200:
            results.append(Result("readyz", True, "readyz 200: every component ready"))
        else:
            down = sorted(name for name, text in state.items() if text != "ready")
            failure = value.get("gateway_failure") or {}
            if failure.get("class") == DB_BEHIND_CLASS:
                results.append(Result("readyz", False,
                                      f"readyz 503: db-behind-binary signature "
                                      f"(gateway_failure durable_gateway:{DB_BEHIND_CLASS}); "
                                      "migrate before redeploying"))
            else:
                results.append(Result("readyz", False,
                                      f"readyz 503: parked ({', '.join(down)} not ready); "
                                      "truthful, not E2E approval"))
    except ProbeError as error:
        results.append(Result("readyz", False, str(error)))
    results.append(check_build(report, args.expected_sha))
    return origin, results, report


def evidence_shape(results, report, expected_sha):
    """Allowlisted receipt: check names, verdicts and shape tokens only."""
    failure = (report or {}).get("gateway_failure") or {}
    return {"checks": [{"check": result.name, "verdict": "pass" if result.ok else "fail",
                        "shape": result.reason} for result in results],
            "build_revision": (report or {}).get("build_revision"),
            "build_id": (report or {}).get("build_id"),
            "gateway_failure_class": failure.get("class"),
            "expected_sha": expected_sha}


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--staging-url",
                        default=os.environ.get("STAGING_WORKER_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL)")
    parser.add_argument("--expected-sha", default=os.environ.get("EXPECTED_SHA"),
                        help="deployed commit SHA the binary must report (default: $EXPECTED_SHA)")
    parser.add_argument("--evidence", default="staging-health-contract-evidence.json",
                        help="receipt path written when the fetch phase completes")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=fetch):
    args = parse_args(argv)
    origin, results, report = run(args, fetch_fn)
    for result in results:
        print(result.line())
    failed = sum(not result.ok for result in results)
    print(f"staging health-contract probe: {len(results) - failed}/{len(results)} checks passed")
    if origin is None:
        # Origin fence refusal: no request was sent, nothing to attest.
        return 2
    receipt = {"origin": origin,
               "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               **evidence_shape(results, report, args.expected_sha),
               "result": "pass" if not failed else "fail"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"staging health-contract probe failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
